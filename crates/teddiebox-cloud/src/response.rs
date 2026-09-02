//! Parses just enough of an HTTP response to decide what to do with it.

use crate::{CloudError, ETag};

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
/// Returning `None` rather than a partial guess is deliberate: a caller given
/// `None` refetches from zero, which is slow but correct, while a caller given
/// a wrong `first` writes the body at the wrong offset and corrupts the file
/// without any error to show for it.
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
/// A `1xx` response is a preamble, not an answer: the real response follows it
/// in the same stream. Each one is skipped, and since every head consumes at
/// least its own terminator the search always advances and ends either at a
/// final status or at a buffer with no complete head left in it.
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
    // The body is arbitrary bytes — Opus, not text — so the terminator is
    // found on the raw slice and only the head is validated as UTF-8.
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
        // Whitespace around the colon is malformed but harmless, and a client
        // that drops a header over it loses real information for no gain.
        let name = name.trim();
        let value = value.trim();
        // "identity" is the one encoding that leaves the body alone.
        if name.eq_ignore_ascii_case("transfer-encoding") && !value.eq_ignore_ascii_case("identity")
        {
            return Err(CloudError::UnsupportedTransferEncoding);
        } else if name.eq_ignore_ascii_case("etag") {
            // The first usable value wins. Assigning unconditionally let a
            // later unusable copy write `None` straight over a good one.
            if etag.is_none() {
                // Too long to store: drop it. The cost is one re-download.
                etag = ETag::try_from(value).ok();
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

    /// A server may answer with `100 Continue` before the real response. Both
    /// end with a blank line, so a parser that stops at the first one reports
    /// the preamble's status and points the body at the response that follows.
    #[test]
    fn an_informational_preamble_is_not_mistaken_for_the_response() {
        let raw = b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nbody";
        let (head, body_at) = parse_head(raw).unwrap();
        assert_eq!(head.status, 200);
        assert_eq!(head.content_length, Some(4));
        assert_eq!(&raw[body_at..], b"body");
    }

    /// Chunked bodies carry their own framing. Nothing here removes it, so
    /// accepting one would splice chunk sizes into the Opus stream — a
    /// corruption that surfaces as noise rather than as an error.
    #[test]
    fn a_chunked_body_is_refused_rather_than_decoded_as_audio() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nbody\r\n0\r\n\r\n";
        assert_eq!(
            parse_head(raw),
            Err(CloudError::UnsupportedTransferEncoding)
        );
    }

    #[test]
    fn a_header_name_padded_with_space_is_still_recognised() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length : 4\r\nETag : \"v1\"\r\n\r\nbody";
        let (head, _) = parse_head(raw).unwrap();
        assert_eq!(head.content_length, Some(4));
        assert_eq!(head.etag.as_deref(), Some("\"v1\""));
    }

    #[test]
    fn parses_a_successful_response() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 4096\r\nETag: \"v1\"\r\n\r\nbody";
        let (head, body_at) = parse_head(raw).unwrap();
        assert_eq!(head.status, 200);
        assert_eq!(head.content_length, Some(4096));
        assert_eq!(head.etag.as_deref(), Some("\"v1\""));
        assert_eq!(&raw[body_at..], b"body");
    }

    #[test]
    fn a_body_that_is_not_text_does_not_make_the_response_malformed() {
        // Opus data is arbitrary bytes. Validating the body as UTF-8 would
        // reject every real download.
        let mut raw = Vec::from(*b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\n");
        raw.extend_from_slice(&[0xFF, 0x00, 0x80, 0x13]);

        let (head, body_at) = parse_head(&raw).unwrap();
        assert_eq!(head.status, 200);
        assert_eq!(&raw[body_at..], &[0xFF, 0x00, 0x80, 0x13]);
    }

    #[test]
    fn parses_a_not_modified_response() {
        let raw = b"HTTP/1.1 304 Not Modified\r\nETag: \"v1\"\r\n\r\n";
        let (head, body_at) = parse_head(raw).unwrap();
        assert_eq!(head.status, 304);
        assert_eq!(body_at, raw.len());
    }

    #[test]
    fn header_names_are_matched_case_insensitively() {
        let raw = b"HTTP/1.1 200 OK\r\ncontent-length: 12\r\netag: \"x\"\r\n\r\n";
        let (head, _) = parse_head(raw).unwrap();
        assert_eq!(head.content_length, Some(12));
        assert_eq!(head.etag.as_deref(), Some("\"x\""));
    }

    #[test]
    fn a_missing_etag_is_absent_rather_than_an_error() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\n";
        let (head, _) = parse_head(raw).unwrap();
        assert_eq!(head.etag, None);
    }

    #[test]
    fn unknown_headers_are_skipped() {
        let raw = b"HTTP/1.1 200 OK\r\nServer: teddyCloud\r\nX-Thing: 1\r\nETag: \"e\"\r\n\r\n";
        let (head, _) = parse_head(raw).unwrap();
        assert_eq!(head.etag.as_deref(), Some("\"e\""));
    }

    #[test]
    fn an_overlong_etag_is_dropped_rather_than_failing_the_request() {
        // 72 characters, over the 64-byte ETag limit.
        const RAW: &[u8] = b"HTTP/1.1 200 OK\r\nETag: xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\r\n\r\n";
        let (head, _) = parse_head(RAW).unwrap();
        assert_eq!(head.etag, None, "a re-download is better than an error");
    }

    /// Each header was assigned with `.ok()`, so a second copy that failed to
    /// parse wrote `None` over a value already read correctly.
    #[test]
    fn a_repeated_unusable_header_does_not_erase_the_value_already_parsed() {
        const RAW: &[u8] = b"HTTP/1.1 200 OK\r\n\
ETag: \"v1\"\r\n\
Content-Length: 4\r\n\
ETag: xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\r\n\
Content-Length: banana\r\n\r\nbody";
        let (head, _) = parse_head(RAW).unwrap();
        assert_eq!(head.etag.as_deref(), Some("\"v1\""));
        assert_eq!(head.content_length, Some(4));
    }

    #[test]
    fn a_response_without_a_status_line_is_malformed() {
        assert_eq!(
            parse_head(b"garbage\r\n\r\n"),
            Err(CloudError::MalformedResponse)
        );
    }

    #[test]
    fn an_incomplete_head_is_malformed() {
        assert_eq!(
            parse_head(b"HTTP/1.1 200 OK\r\nETag: \"e\"\r\n"),
            Err(CloudError::MalformedResponse)
        );
    }

    #[test]
    fn a_partial_response_reports_where_its_body_starts_and_how_long_the_file_is() {
        let raw = b"HTTP/1.1 206 Partial Content\r\n\
Content-Range: bytes 4096-27841284/27841285\r\n\
Content-Length: 27837189\r\n\r\nDATA";
        let (head, body_at) = parse_head(raw).unwrap();
        assert_eq!(head.status, 206);
        let range = head.content_range.unwrap();
        assert_eq!(range.first, 4096);
        assert_eq!(range.last, 27841284);
        assert_eq!(range.total, Some(27841285));
        assert_eq!(&raw[body_at..], b"DATA");
    }

    /// A server that cannot say how long the whole file is writes `*`. The
    /// range is still usable for placing the bytes; only the completeness
    /// check loses its number, and the caller decides what to do about that.
    #[test]
    fn an_unknown_total_is_absent_rather_than_an_error() {
        let raw = b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-3/*\r\n\r\nDATA";
        let (head, _) = parse_head(raw).unwrap();
        assert_eq!(head.content_range.unwrap().total, None);
    }

    /// A malformed range is dropped rather than half-parsed. A caller that
    /// sees `None` refetches from zero, which is correct but slow; a caller
    /// handed a wrong `first` writes the body at the wrong offset, which
    /// corrupts the file silently.
    #[test]
    fn a_malformed_content_range_is_dropped_rather_than_half_parsed() {
        let raw = b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes garbage\r\n\r\n";
        let (head, _) = parse_head(raw).unwrap();
        assert_eq!(head.content_range, None);
    }

    #[test]
    fn a_response_with_no_content_range_reports_none() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nbody";
        let (head, _) = parse_head(raw).unwrap();
        assert_eq!(head.content_range, None);
    }
}
