//! Builds teddyCloud HTTP requests into a caller-supplied buffer.
//!
//! Plain HTTP by design while the server is LAN-local. Nothing here knows how
//! the bytes reach the network, so adding TLS is a change of transport only.

use crate::CloudError;
use core::fmt::Write;
use heapless::String;

/// Longest ETag we will echo back. Longer ones are dropped, costing a
/// re-download rather than an error.
pub const MAX_ETAG: usize = 64;

pub type ETag = String<MAX_ETAG>;

/// Everything that varies between one content request and the next.
///
/// A struct rather than a parameter list because resuming adds fields, and a
/// six-argument function is where a caller silently transposes two of them.
#[derive(Debug, Clone)]
pub struct ContentRequest<'a> {
    pub uid: [u8; 8],
    /// Echoed back as `If-None-Match`, making the request conditional.
    pub etag: Option<&'a ETag>,
    /// `host:port` of the teddyCloud server.
    pub server: &'a str,
}

/// Writes a conditional GET for the content of `tag` into `out`.
///
/// When `etag` is present the request is conditional, so an unchanged file
/// answers 304 and costs nothing but headers — which is what lets the box
/// revalidate cached content without interrupting playback.
pub fn build_content_request(
    request: &ContentRequest<'_>,
    out: &mut [u8],
) -> Result<usize, CloudError> {
    let mut buf = SliceWriter { out, used: 0 };

    write!(buf, "GET /content/").map_err(|_| CloudError::RequestTooLong)?;
    for b in request.uid {
        write!(buf, "{b:02X}").map_err(|_| CloudError::RequestTooLong)?;
    }
    let server = request.server;
    write!(buf, " HTTP/1.1\r\nHost: {server}\r\n").map_err(|_| CloudError::RequestTooLong)?;

    if let Some(tag) = request.etag {
        write!(buf, "If-None-Match: {tag}\r\n").map_err(|_| CloudError::RequestTooLong)?;
    }

    write!(buf, "Connection: close\r\n\r\n").map_err(|_| CloudError::RequestTooLong)?;

    Ok(buf.used)
}

/// Formats straight into the caller's buffer.
///
/// The request used to be built in a `String<512>` and copied out, which cost
/// the stack both buffers and capped the request at 512 bytes however large
/// `out` was. Running out of room is the caller's `RequestTooLong` either way.
struct SliceWriter<'a> {
    out: &'a mut [u8],
    used: usize,
}

impl Write for SliceWriter<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let end = self.used + s.len();
        if end > self.out.len() {
            return Err(core::fmt::Error);
        }
        self.out[self.used..end].copy_from_slice(s.as_bytes());
        self.used = end;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UID: [u8; 8] = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08];

    fn build(etag: Option<&ETag>) -> heapless::String<512> {
        let mut out = [0u8; 512];
        let request = ContentRequest {
            uid: UID,
            etag,
            server: "box.lan:8080",
        };
        let n = build_content_request(&request, &mut out).unwrap();
        heapless::String::try_from(core::str::from_utf8(&out[..n]).unwrap()).unwrap()
    }

    #[test]
    fn the_uid_is_uppercase_hex_in_the_path() {
        let req = build(None);
        assert!(
            req.starts_with("GET /content/0102030405060708 HTTP/1.1\r\n"),
            "got: {req}"
        );
    }

    #[test]
    fn the_host_header_carries_the_configured_server() {
        let req = build(None);
        assert!(req.contains("Host: box.lan:8080\r\n"));
    }

    #[test]
    fn without_an_etag_the_request_is_unconditional() {
        let req = build(None);
        assert!(!req.contains("If-None-Match"));
    }

    #[test]
    fn with_an_etag_the_request_is_conditional() {
        let etag = ETag::try_from("\"abc123\"").unwrap();
        let req = build(Some(&etag));
        assert!(req.contains("If-None-Match: \"abc123\"\r\n"));
    }

    #[test]
    fn the_request_ends_with_a_blank_line() {
        assert!(build(None).ends_with("\r\n\r\n"));
    }

    /// The old builder copied through a fixed 512-byte string, so a request
    /// longer than that failed however much room the caller offered.
    #[test]
    fn the_request_length_is_bounded_by_the_callers_buffer_alone() {
        let server = "x".repeat(600);
        let mut out = [0u8; 1024];
        let request = ContentRequest {
            uid: UID,
            etag: None,
            server: &server,
        };
        let n = build_content_request(&request, &mut out).unwrap();
        assert!(n > 512, "built {n} bytes");
        assert!(out[..n].ends_with(b"\r\n\r\n"));
    }

    #[test]
    fn a_buffer_too_small_is_an_error_rather_than_a_truncated_request() {
        let mut out = [0u8; 8];
        let request = ContentRequest {
            uid: UID,
            etag: None,
            server: "box.lan:8080",
        };
        assert_eq!(
            build_content_request(&request, &mut out),
            Err(CloudError::RequestTooLong)
        );
    }
}
