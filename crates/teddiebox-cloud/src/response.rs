//! Parses just enough of an HTTP response to decide what to do with it.

use crate::{request::parse_etag, CloudError, ETag};

/// The `Content-Range` of a `206`, which is the only place a resumed response
/// states the file's full length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContentRange {
    /// Offset of the first byte of this body within the whole file.
    pub first: u32,
    /// Offset of its last byte, inclusive.
    pub last: u32,
    /// Length of the whole file, when the server chose to say.
    pub total: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponseHead {
    pub status: u16,
    pub etag: Option<ETag>,
    pub content_length: Option<u32>,
    pub content_range: Option<ContentRange>,
}

/// Parses `bytes 4096-27841284/27841285`, or `None` if it is not that shape.
///
/// Returns `None` rather than a partial guess: with `None` the caller
/// downloads from zero, which is slow but correct, while a wrong `first`
/// would silently write the body at the wrong offset.
fn parse_content_range(value: &str) -> Option<ContentRange> {
    let rest = value.strip_prefix("bytes ")?;
    let (range, total) = rest.split_once('/')?;
    let (first, last) = range.split_once('-')?;
    Some(ContentRange {
        first: first.trim().parse().ok()?,
        last: last.trim().parse().ok()?,
        total: if total.trim() == "*" {
            None
        } else {
            Some(total.trim().parse().ok()?)
        },
    })
}

/// Parses the head of a response. Returns the parsed fields and the offset at
/// which the body starts.
///
/// A `1xx` response is only a preamble; the real response follows it. Each
/// one is skipped. Every head includes its terminator, so the loop always
/// advances and ends at a final status or at an incomplete head.
pub fn parse_head(buf: &[u8]) -> Result<(ResponseHead, usize), CloudError> {
    let mut consumed = 0;
    loop {
        let (head, body_at) = parse_one_head(&buf[consumed..])?;
        consumed += body_at;
        if !(100..200).contains(&head.status) {
            return Ok((head, consumed));
        }
    }
}

fn parse_one_head(buf: &[u8]) -> Result<(ResponseHead, usize), CloudError> {
    // The body is binary (Opus), so the terminator is found in the raw bytes
    // and only the head is checked as UTF-8.
    let head_end = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or(CloudError::MalformedResponse)?;
    let body_at = head_end + 4;

    let text = core::str::from_utf8(&buf[..head_end]).map_err(|_| CloudError::MalformedResponse)?;

    let mut lines = text.split("\r\n");

    let status_line = lines.next().ok_or(CloudError::MalformedResponse)?;
    let mut parts = status_line.split(' ');
    let version = parts.next().unwrap_or_default();
    if !version.starts_with("HTTP/") {
        return Err(CloudError::MalformedResponse);
    }
    let status: u16 = parts
        .next()
        .ok_or(CloudError::MalformedResponse)?
        .parse()
        .map_err(|_| CloudError::MalformedResponse)?;

    let mut etag = None;
    let mut content_length = None;
    let mut content_range = None;

    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        // Whitespace around the colon is not allowed but harmless, so accept
        // it.
        let name = name.trim();
        let value = value.trim();
        // "identity" is the only encoding that leaves the body unchanged.
        if name.eq_ignore_ascii_case("transfer-encoding") && !value.eq_ignore_ascii_case("identity")
        {
            return Err(CloudError::UnsupportedTransferEncoding);
        } else if name.eq_ignore_ascii_case("etag") {
            // The first usable value wins, so a later unusable copy cannot
            // replace a good one with `None`.
            if etag.is_none() {
                // Too long, or containing a byte not allowed in a header
                // value: drop it. The cost is one re-download.
                etag = parse_etag(value);
            }
        } else if name.eq_ignore_ascii_case("content-length") && content_length.is_none() {
            content_length = value.parse().ok();
        } else if name.eq_ignore_ascii_case("content-range") && content_range.is_none() {
            content_range = parse_content_range(value);
        }
    }

    Ok((
        ResponseHead {
            status,
            etag,
            content_length,
            content_range,
        },
        body_at,
    ))
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::vec::Vec;

    use super::*;

    /// A server may send `100 Continue` before the real response. Both end
    /// with a blank line, so the parser must skip the first one.
    #[test]
    fn an_informational_preamble_is_not_mistaken_for_the_response() {
        // Given
        let raw = b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nbody";

        // When
        let (head, body_at) = parse_head(raw).unwrap();

        // Then
        assert_eq!(head.status, 200);
        assert_eq!(head.content_length, Some(4));
        assert_eq!(&raw[body_at..], b"body");
    }

    /// Chunked bodies contain chunk sizes. Nothing here removes them, so
    /// accepting one would put chunk sizes into the Opus stream as noise.
    #[test]
    fn a_chunked_body_is_refused_rather_than_decoded_as_audio() {
        // Given
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nbody\r\n0\r\n\r\n";

        // When
        let parsed = parse_head(raw);

        // Then
        assert_eq!(parsed, Err(CloudError::UnsupportedTransferEncoding));
    }

    #[test]
    fn a_header_name_padded_with_space_is_still_recognised() {
        // Given
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length : 4\r\nETag : \"v1\"\r\n\r\nbody";

        // When
        let (head, _) = parse_head(raw).unwrap();

        // Then
        assert_eq!(head.content_length, Some(4));
        assert_eq!(head.etag.as_deref(), Some("\"v1\""));
    }

    #[test]
    fn parses_a_successful_response() {
        // Given
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 4096\r\nETag: \"v1\"\r\n\r\nbody";

        // When
        let (head, body_at) = parse_head(raw).unwrap();

        // Then
        assert_eq!(head.status, 200);
        assert_eq!(head.content_length, Some(4096));
        assert_eq!(head.etag.as_deref(), Some("\"v1\""));
        assert_eq!(&raw[body_at..], b"body");
    }

    #[test]
    fn a_body_that_is_not_text_does_not_make_the_response_malformed() {
        // Given: Opus data is binary; checking it as UTF-8 would reject every real
        // download.
        let mut raw = Vec::from(*b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\n");
        raw.extend_from_slice(&[0xFF, 0x00, 0x80, 0x13]);

        // When
        let (head, body_at) = parse_head(&raw).unwrap();

        // Then
        assert_eq!(head.status, 200);
        assert_eq!(&raw[body_at..], &[0xFF, 0x00, 0x80, 0x13]);
    }

    #[test]
    fn parses_a_not_modified_response() {
        // Given
        let raw = b"HTTP/1.1 304 Not Modified\r\nETag: \"v1\"\r\n\r\n";

        // When
        let (head, body_at) = parse_head(raw).unwrap();

        // Then
        assert_eq!(head.status, 304);
        assert_eq!(body_at, raw.len());
    }

    #[test]
    fn header_names_are_matched_case_insensitively() {
        // Given
        let raw = b"HTTP/1.1 200 OK\r\ncontent-length: 12\r\netag: \"x\"\r\n\r\n";

        // When
        let (head, _) = parse_head(raw).unwrap();

        // Then
        assert_eq!(head.content_length, Some(12));
        assert_eq!(head.etag.as_deref(), Some("\"x\""));
    }

    #[test]
    fn a_missing_etag_is_absent_rather_than_an_error() {
        // Given
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\n";

        // When
        let (head, _) = parse_head(raw).unwrap();

        // Then
        assert_eq!(head.etag, None);
    }

    #[test]
    fn unknown_headers_are_skipped() {
        // Given
        let raw = b"HTTP/1.1 200 OK\r\nServer: teddyCloud\r\nX-Thing: 1\r\nETag: \"e\"\r\n\r\n";

        // When
        let (head, _) = parse_head(raw).unwrap();

        // Then
        assert_eq!(head.etag.as_deref(), Some("\"e\""));
    }

    #[test]
    fn an_overlong_etag_is_dropped_rather_than_failing_the_request() {
        // Given: 72 characters, over the 64-byte ETag limit.
        const RAW: &[u8] = b"HTTP/1.1 200 OK\r\nETag: xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\r\n\r\n";

        // When
        let (head, _) = parse_head(RAW).unwrap();

        // Then
        assert_eq!(head.etag, None, "a re-download is better than an error");
    }

    /// The value is copied without escaping into `If-None-Match:` and
    /// `If-Range:` headers and into the `.MET` sidecar. A bare `CR` survives
    /// the `\r\n` split, so a server could inject a line break into our next
    /// request.
    #[test]
    fn an_etag_carrying_a_bare_cr_is_dropped() {
        // Given
        const RAW: &[u8] = b"HTTP/1.1 200 OK\r\nETag: \"v1\rX-Thing: 1\"\r\n\r\n";

        // When
        let (head, _) = parse_head(RAW).unwrap();

        // Then
        assert_eq!(head.etag, None, "a re-download is better than an injection");
    }

    /// A second copy of a header that fails to parse must not replace a good
    /// first value with `None`.
    #[test]
    fn a_repeated_unusable_header_does_not_erase_the_value_already_parsed() {
        // Given
        const RAW: &[u8] = b"HTTP/1.1 200 OK\r\n\
ETag: \"v1\"\r\n\
Content-Length: 4\r\n\
ETag: xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\r\n\
Content-Length: banana\r\n\r\nbody";

        // When
        let (head, _) = parse_head(RAW).unwrap();

        // Then
        assert_eq!(head.etag.as_deref(), Some("\"v1\""));
        assert_eq!(head.content_length, Some(4));
    }

    /// The first usable value wins, as for `ETag`: a later copy does not
    /// change how much of the body is read.
    #[test]
    fn a_repeated_content_length_keeps_the_first_value() {
        // Given
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nContent-Length: 9\r\n\r\nbody";

        // When
        let (head, _) = parse_head(raw).unwrap();

        // Then
        assert_eq!(head.content_length, Some(4));
    }

    #[test]
    fn a_response_without_a_status_line_is_malformed() {
        // Given
        let raw = b"garbage\r\n\r\n";

        // When
        let parsed = parse_head(raw);

        // Then
        assert_eq!(parsed, Err(CloudError::MalformedResponse));
    }

    #[test]
    fn an_incomplete_head_is_malformed() {
        // Given
        let raw = b"HTTP/1.1 200 OK\r\nETag: \"e\"\r\n";

        // When
        let parsed = parse_head(raw);

        // Then
        assert_eq!(parsed, Err(CloudError::MalformedResponse));
    }

    #[test]
    fn a_partial_response_reports_where_its_body_starts_and_how_long_the_file_is() {
        // Given
        let raw = b"HTTP/1.1 206 Partial Content\r\n\
Content-Range: bytes 4096-27841284/27841285\r\n\
Content-Length: 27837189\r\n\r\nDATA";

        // When
        let (head, body_at) = parse_head(raw).unwrap();

        // Then
        assert_eq!(head.status, 206);
        let range = head.content_range.unwrap();
        assert_eq!(range.first, 4096);
        assert_eq!(range.last, 27841284);
        assert_eq!(range.total, Some(27841285));
        assert_eq!(&raw[body_at..], b"DATA");
    }

    /// A server that does not know the file's length writes `*`. The range
    /// can still place the bytes; only the total is missing.
    #[test]
    fn an_unknown_total_is_absent_rather_than_an_error() {
        // Given
        let raw = b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-3/*\r\n\r\nDATA";

        // When
        let (head, _) = parse_head(raw).unwrap();

        // Then
        assert_eq!(head.content_range.unwrap().total, None);
    }

    /// A malformed range is dropped, not half-parsed. With `None` the caller
    /// downloads from zero (slow but correct); a wrong `first` would silently
    /// corrupt the file.
    #[test]
    fn a_malformed_content_range_is_dropped_rather_than_half_parsed() {
        // Given
        let raw = b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes garbage\r\n\r\n";

        // When
        let (head, _) = parse_head(raw).unwrap();

        // Then
        assert_eq!(head.content_range, None);
    }

    #[test]
    fn a_response_with_no_content_range_reports_none() {
        // Given
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nbody";

        // When
        let (head, _) = parse_head(raw).unwrap();

        // Then
        assert_eq!(head.content_range, None);
    }
}
