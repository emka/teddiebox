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
    /// Echoed back as `If-None-Match` on a fresh request, or as `If-Range`
    /// when resuming.
    pub etag: Option<&'a ETag>,
    /// `host:port` of the teddyCloud server.
    pub server: &'a str,
    /// Where to resume. `Some(n)` asks for `bytes=n-`.
    pub from: Option<u32>,
}

/// Writes a GET for the content of `tag` into `out`.
///
/// A fresh request with an etag is conditional: an unchanged file answers 304,
/// which revalidates the cache without interrupting playback. A resumed request
/// is always a range request — the response is 206 (continue) or 200 (restart),
/// never 304. This distinction matters: a 304 says nothing about whether the
/// partial bytes already on the card are still the right prefix of the new
/// content.
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

    match (request.from, request.etag) {
        (Some(from), etag) => {
            write!(buf, "Range: bytes={from}-\r\n").map_err(|_| CloudError::RequestTooLong)?;
            if let Some(tag) = etag {
                write!(buf, "If-Range: {tag}\r\n").map_err(|_| CloudError::RequestTooLong)?;
            }
        }
        (None, Some(tag)) => {
            write!(buf, "If-None-Match: {tag}\r\n").map_err(|_| CloudError::RequestTooLong)?;
        }
        (None, None) => {}
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
            from: None,
        };
        let n = build_content_request(&request, &mut out).unwrap();
        heapless::String::try_from(core::str::from_utf8(&out[..n]).unwrap()).unwrap()
    }

    fn build_from(from: Option<u32>, etag: Option<&ETag>) -> heapless::String<512> {
        let mut out = [0u8; 512];
        let request = ContentRequest {
            uid: UID,
            etag,
            server: "box.lan:8080",
            from,
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
            from: None,
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
            from: None,
        };
        assert_eq!(
            build_content_request(&request, &mut out),
            Err(CloudError::RequestTooLong)
        );
    }

    #[test]
    fn a_resumed_download_asks_for_the_bytes_it_is_missing() {
        let req = build_from(Some(4096), None);
        assert!(req.contains("Range: bytes=4096-\r\n"), "got: {req}");
    }

    /// `If-None-Match` alongside a `Range` invites a 304, which says nothing
    /// about whether the partial file on the card is still the right prefix.
    /// `If-Range` answers exactly the two questions a resume has: continue
    /// (206), or start again (200).
    #[test]
    fn a_resumed_download_validates_with_if_range_not_if_none_match() {
        let etag = ETag::try_from("\"v1\"").unwrap();
        let req = build_from(Some(4096), Some(&etag));
        assert!(req.contains("If-Range: \"v1\"\r\n"), "got: {req}");
        assert!(!req.contains("If-None-Match"), "got: {req}");
    }

    #[test]
    fn a_fresh_download_asks_for_no_range() {
        let req = build_from(None, None);
        assert!(!req.contains("Range:"), "got: {req}");
    }

    /// Without a range, an etag is still a revalidation and still belongs in
    /// `If-None-Match`.
    #[test]
    fn without_a_range_an_etag_is_still_a_conditional_get() {
        let etag = ETag::try_from("\"v1\"").unwrap();
        let req = build_from(None, Some(&etag));
        assert!(req.contains("If-None-Match: \"v1\"\r\n"), "got: {req}");
        assert!(!req.contains("If-Range"), "got: {req}");
    }

    #[test]
    fn a_resume_from_zero_is_still_a_range_request() {
        // Zero is a legitimate resume point after a sidecar was written but
        // no body byte ever landed. Treating it as "no range" would send an
        // unconditional GET and silently discard the etag check.
        let req = build_from(Some(0), None);
        assert!(req.contains("Range: bytes=0-\r\n"), "got: {req}");
    }
}
