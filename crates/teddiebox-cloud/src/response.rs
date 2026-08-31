//! Parses just enough of an HTTP response to decide what to do with it.

use crate::{CloudError, ETag};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponseHead {
    pub status: u16,
    pub etag: Option<ETag>,
    pub content_length: Option<u32>,
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

    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("transfer-encoding") {
            // "identity" is the one encoding that leaves the body alone.
            if !value.eq_ignore_ascii_case("identity") {
                return Err(CloudError::UnsupportedTransferEncoding);
            }
        } else if name.eq_ignore_ascii_case("etag") {
            // Too long to store: drop it. The cost is one re-download.
            etag = ETag::try_from(value).ok();
        } else if name.eq_ignore_ascii_case("content-length") {
            content_length = value.parse().ok();
        }
    }

    Ok((
        ResponseHead {
            status,
            etag,
            content_length,
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
        assert_eq!(parse_head(raw), Err(CloudError::UnsupportedTransferEncoding));
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
}
