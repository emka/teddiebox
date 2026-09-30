//! Builds teddyCloud HTTP requests into a caller-supplied buffer.
//!
//! Nothing here knows how the bytes reach the network; the firmware sends
//! them over TLS.

use crate::CloudError;
use core::fmt::Write;
use heapless::String;

/// Longest ETag we echo back. Longer ones are dropped, which costs a
/// re-download, not an error.
pub const MAX_ETAG: usize = 64;

pub type ETag = String<MAX_ETAG>;

/// Reads a server's ETag header value, or `None` if it is unusable.
///
/// The value is copied unescaped into `If-None-Match:` and `If-Range:`
/// headers by [`write_request`], and into the `.MET` sidecar. Only printable,
/// non-space ASCII is allowed (what RFC 9110 allows in an entity-tag), so a
/// server cannot inject a `CR`, `LF` or space into our next request.
///
/// An unusable value is dropped, like an overlong one: the cost is one
/// re-download.
pub fn parse_etag(value: &str) -> Option<ETag> {
    if !value.bytes().all(|b| b.is_ascii_graphic()) {
        return None;
    }
    ETag::try_from(value).ok()
}

/// Everything that varies between one content request and the next.
///
/// A struct rather than many parameters, so a caller cannot swap two of them
/// by mistake.
#[derive(Debug, Clone)]
pub struct ContentRequest<'a> {
    pub uid: [u8; 8],
    /// Which content route to ask.
    pub route: Route,
    /// Echoed back as `If-None-Match` on a fresh request, or as `If-Range`
    /// when resuming.
    pub etag: Option<&'a ETag>,
    /// `host:port` of the teddyCloud server.
    pub server: &'a str,
    /// Where to resume. `Some(n)` asks for `bytes=n-`.
    pub from: Option<u32>,
    /// The tag's own memory, sent as `Authorization: BD <64 hex>`.
    ///
    /// teddyCloud does not check this itself. It forwards it to the Tonies
    /// cloud when it does not already have the content, and the cloud
    /// accepts or rejects it. Without it, a figure the server does not have
    /// gets a `403`.
    ///
    /// `None` for content the server already has, which needs no token.
    pub auth: Option<&'a [u8; TOKEN_BYTES]>,
}

/// Length of a tag's authentication token: the whole of its user memory.
///
/// `TONIE_AUTH_TOKEN_LENGTH` in teddyCloud's `include/net_config.h`, which
/// equals eight four-byte ICODE SLIX-L blocks.
pub const TOKEN_BYTES: usize = 32;

/// Which of teddyCloud's two content routes to ask.
///
/// [`Route::V2`] is the stock box's endpoint and the default. [`Route::V1`]
/// exists because our teddyCloud server does not answer on `/v2`: it accepts
/// the connection and then sends nothing until the client times out, with or
/// without an `Authorization` header. `/v1` returns the file. teddyCloud's
/// source sets `noPassword = TRUE` for `/v1`, which may be the difference;
/// the hang is not understood, and this only works around it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Route {
    V1,
    #[default]
    V2,
}

impl Route {
    const fn path(self) -> &'static str {
        match self {
            Route::V1 => "/v1/content/",
            Route::V2 => "/v2/content/",
        }
    }
}

/// Writes a GET for the content of `tag` into `out`.
///
/// The path is `/v2/content/<ruid>` (or `/v1`, see [`Route`]): teddyCloud's
/// route for boxes (`handleCloudContentV2` in its `handler_cloud.c`). Not
/// `/content/`, which is its web admin API.
///
/// `<ruid>` is the UID with its bytes reversed: teddyCloud byte-swaps it back
/// (`bswap_64` in `handler_cloud.c`). Sending the UID unreversed would look
/// valid but ask for a different tag.
///
/// A fresh request with an etag is conditional: an unchanged file gets a
/// 304. A resumed request is always a range request, answered with 206
/// (continue) or 200 (start again), never 304, because a 304 would not say
/// whether the partial file on the card is still valid.
pub fn build_content_request(
    request: &ContentRequest<'_>,
    out: &mut [u8],
) -> Result<usize, CloudError> {
    write_request(request, Ask::Content, out)
}

/// Writes a GET for a file at `path` into `out`.
///
/// Used for firmware updates, which fetch a path (`/teddiebox.txt`, then an
/// image) rather than a figure's content, so there is no uid, route or
/// token.
///
/// `from: Some(n)` asks for everything from byte `n`, like a resumed content
/// request. `None` asks for the whole file. There is no etag, because nothing
/// needs it yet.
///
/// Unlike `build_content_request`, no `Connection: close`: teddyCloud's
/// `/content/` handler (CycloneHTTP) writes `Content-Length` only on a
/// persistent connection, and without it the response ends at the close,
/// which `begin_prepared` refuses as `LengthRequired`. The caller closes the
/// session once the body is read.
pub fn build_path_request(
    buf: &mut [u8],
    path: &str,
    server: &str,
    from: Option<u32>,
) -> Result<usize, CloudError> {
    let mut writer = SliceWriter { out: buf, used: 0 };

    write!(writer, "GET {path} HTTP/1.1\r\nHost: {server}\r\n")
        .map_err(|_| CloudError::RequestTooLong)?;

    if let Some(from) = from {
        write!(writer, "Range: bytes={from}-\r\n").map_err(|_| CloudError::RequestTooLong)?;
    }

    write!(writer, "\r\n").map_err(|_| CloudError::RequestTooLong)?;

    Ok(writer.used)
}

/// Which byte a length probe asks for.
///
/// One, not zero. teddyCloud answers a range starting at zero with `200` and
/// the whole file, without `Content-Range`. A range at one gets `206` with
/// `Content-Range: bytes 1-1/<total>`, which gives the length.
const PROBE_AT: u32 = 1;

/// Builds a request for one byte, to learn how long the whole file is.
///
/// **`HEAD` does not work**: teddyCloud answers it with `404` where `GET`
/// answers `200`. So the length comes from the `Content-Range` of a partial
/// response, which the resume code already parses.
///
/// Sends the token like a download does, because teddyCloud forwards it
/// upstream for content it does not have. Sends no conditional header, since
/// a `304` has no length.
pub fn build_length_probe(
    request: &ContentRequest<'_>,
    out: &mut [u8],
) -> Result<usize, CloudError> {
    write_request(request, Ask::Length, out)
}

/// What a request is for. The two differ only in the `Range` header.
/// Everything else must be identical, or a probe would ask about a different
/// file than the download that follows it.
enum Ask {
    Content,
    Length,
}

fn write_request(
    request: &ContentRequest<'_>,
    ask: Ask,
    out: &mut [u8],
) -> Result<usize, CloudError> {
    let mut buf = SliceWriter { out, used: 0 };

    write!(buf, "GET {}", request.route.path()).map_err(|_| CloudError::RequestTooLong)?;
    // Lower case on purpose: see `the_identifier_is_written_in_lower_case`.
    for b in request.uid.iter().rev() {
        write!(buf, "{b:02x}").map_err(|_| CloudError::RequestTooLong)?;
    }
    let server = request.server;
    write!(buf, " HTTP/1.1\r\nHost: {server}\r\n").map_err(|_| CloudError::RequestTooLong)?;

    match (ask, request.from, request.etag) {
        (Ask::Length, _, _) => {
            write!(buf, "Range: bytes={PROBE_AT}-{PROBE_AT}\r\n")
                .map_err(|_| CloudError::RequestTooLong)?;
        }
        (Ask::Content, Some(from), etag) => {
            write!(buf, "Range: bytes={from}-\r\n").map_err(|_| CloudError::RequestTooLong)?;
            if let Some(tag) = etag {
                write!(buf, "If-Range: {tag}\r\n").map_err(|_| CloudError::RequestTooLong)?;
            }
        }
        (Ask::Content, None, Some(tag)) => {
            write!(buf, "If-None-Match: {tag}\r\n").map_err(|_| CloudError::RequestTooLong)?;
        }
        (Ask::Content, None, None) => {}
    }

    if let Some(token) = request.auth {
        write!(buf, "Authorization: BD ").map_err(|_| CloudError::RequestTooLong)?;
        for byte in token {
            write!(buf, "{byte:02X}").map_err(|_| CloudError::RequestTooLong)?;
        }
        write!(buf, "\r\n").map_err(|_| CloudError::RequestTooLong)?;
    }

    write!(buf, "Connection: close\r\n\r\n").map_err(|_| CloudError::RequestTooLong)?;

    Ok(buf.used)
}

/// Formats straight into the caller's buffer, with no intermediate copy.
/// Running out of room becomes `RequestTooLong`.
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

    /// A UID whose reversed hex starts with a zero byte, to catch a reversal
    /// that drops leading zeros: the result must be `0077665544332211`, not
    /// `77665544332211`.
    const UID_WITH_LEADING_ZERO_WHEN_REVERSED: [u8; 8] =
        [0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x00];

    fn build(etag: Option<&ETag>) -> heapless::String<512> {
        let mut out = [0u8; 512];
        let request = ContentRequest {
            uid: UID,
            route: Route::V2,
            etag,
            server: "box.lan:8080",
            from: None,
            auth: None,
        };
        let n = build_content_request(&request, &mut out).unwrap();
        heapless::String::try_from(core::str::from_utf8(&out[..n]).unwrap()).unwrap()
    }

    fn build_from(from: Option<u32>, etag: Option<&ETag>) -> heapless::String<512> {
        let mut out = [0u8; 512];
        let request = ContentRequest {
            uid: UID,
            route: Route::V2,
            etag,
            server: "box.lan:8080",
            from,
            auth: None,
        };
        let n = build_content_request(&request, &mut out).unwrap();
        heapless::String::try_from(core::str::from_utf8(&out[..n]).unwrap()).unwrap()
    }

    fn probe(auth: Option<&[u8; TOKEN_BYTES]>) -> heapless::String<512> {
        let mut out = [0u8; 512];
        let request = ContentRequest {
            uid: UID,
            route: Route::V1,
            etag: None,
            server: "box.lan:8080",
            from: None,
            auth,
        };
        let n = build_length_probe(&request, &mut out).unwrap();
        heapless::String::try_from(core::str::from_utf8(&out[..n]).unwrap()).unwrap()
    }

    /// Based on how teddyCloud actually responds:
    ///
    /// - `HEAD` answers **404** where `GET` answers 200.
    /// - `Range: bytes=0-0` answers **200 with the whole body** and no
    ///   `Content-Range`.
    /// - `Range: bytes=1-1` answers **206** with
    ///   `Content-Range: bytes 1-1/<total>` and one byte of body.
    ///
    /// So the probe asks for byte one only.
    #[test]
    fn a_length_probe_asks_for_the_second_byte_and_nothing_else() {
        let request = probe(None);
        assert!(request.contains("Range: bytes=1-1\r\n"), "got: {request}");
        assert!(!request.contains("bytes=0"), "got: {request}");
    }

    /// A probe must not get a `304`, which has no length.
    #[test]
    fn a_length_probe_sends_no_conditional_header() {
        let request = probe(None);
        assert!(!request.contains("If-None-Match"), "got: {request}");
        assert!(!request.contains("If-Range"), "got: {request}");
    }

    /// The same route and lower-case ruid as the download that may follow.
    #[test]
    fn a_length_probe_names_the_same_file_a_fetch_would() {
        let request = probe(None);
        assert!(
            request.starts_with("GET /v1/content/0807060504030201 HTTP/1.1\r\n"),
            "got: {request}"
        );
    }

    /// teddyCloud forwards the token upstream for content it does not have,
    /// which is likely for a figure being probed.
    #[test]
    fn a_length_probe_carries_the_tag_token_when_there_is_one() {
        let token = [0xABu8; TOKEN_BYTES];
        let request = probe(Some(&token));
        assert!(request.contains("Authorization: BD ABAB"), "got: {request}");
    }

    fn build_with_uid(uid: [u8; 8]) -> heapless::String<512> {
        let mut out = [0u8; 512];
        let request = ContentRequest {
            uid,
            route: Route::V2,
            etag: None,
            server: "box.lan:8080",
            from: None,
            auth: None,
        };
        let n = build_content_request(&request, &mut out).unwrap();
        heapless::String::try_from(core::str::from_utf8(&out[..n]).unwrap()).unwrap()
    }

    #[test]
    fn the_route_is_the_box_facing_v2_content_endpoint() {
        let req = build(None);
        assert!(
            req.starts_with("GET /v2/content/"),
            "got: {req}; /content/ is teddyCloud's web-admin route, not the box's"
        );
    }

    #[test]
    fn the_identifier_is_the_byte_reversed_uid_in_hex() {
        // UID bytes 01 02 03 04 05 06 07 08 reversed is 08 07 06 05 04 03 02 01.
        let req = build(None);
        assert!(
            req.starts_with("GET /v2/content/0807060504030201 HTTP/1.1\r\n"),
            "got: {req}"
        );
    }

    #[test]
    fn reversing_the_uid_keeps_a_leading_zero_byte_visible() {
        let req = build_with_uid(UID_WITH_LEADING_ZERO_WHEN_REVERSED);
        assert!(
            req.starts_with("GET /v2/content/0077665544332211 HTTP/1.1\r\n"),
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

    /// The only size limit is the caller's buffer.
    #[test]
    fn the_request_length_is_bounded_by_the_callers_buffer_alone() {
        let server = "x".repeat(600);
        let mut out = [0u8; 1024];
        let request = ContentRequest {
            uid: UID,
            route: Route::V2,
            etag: None,
            server: &server,
            from: None,
            auth: None,
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
            route: Route::V2,
            etag: None,
            server: "box.lan:8080",
            from: None,
            auth: None,
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

    /// `If-None-Match` with a `Range` could get a 304, which does not say
    /// whether the partial file on the card is still valid. `If-Range` gets
    /// either 206 (continue) or 200 (start again).
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
        // Zero is a valid resume point if the sidecar was written but no body
        // byte arrived. Treating it as "no range" would drop the etag check.
        let req = build_from(Some(0), None);
        assert!(req.contains("Range: bytes=0-\r\n"), "got: {req}");
    }
    /// `/v1` works around a teddyCloud that hangs on `/v2` (see [`Route`]).
    /// The default stays `/v2`, the stock box's endpoint.
    #[test]
    fn the_v1_route_is_written_when_it_is_asked_for() {
        let mut out = [0u8; 512];
        let request = ContentRequest {
            uid: UID,
            etag: None,
            server: "box.lan:8080",
            from: None,
            auth: None,
            route: Route::V1,
        };
        let n = build_content_request(&request, &mut out).unwrap();
        let text = core::str::from_utf8(&out[..n]).unwrap();
        assert!(
            text.starts_with("GET /v1/content/"),
            "wrote: {}",
            text.lines().next().unwrap_or("")
        );
    }

    /// The default route is `/v2`.
    #[test]
    fn the_default_route_is_still_v2() {
        let mut out = [0u8; 512];
        let request = ContentRequest {
            uid: UID,
            etag: None,
            server: "box.lan:8080",
            from: None,
            auth: None,
            route: Route::default(),
        };
        let n = build_content_request(&request, &mut out).unwrap();
        let text = core::str::from_utf8(&out[..n]).unwrap();
        assert!(text.starts_with("GET /v2/content/"), "wrote: {text}");
    }
    /// This header lets teddyCloud fetch a figure it does not have: it
    /// forwards the token to the Tonies cloud, which accepts or rejects it.
    ///
    /// Upper-case hex, all thirty-two bytes: teddyCloud's `src/server.c` reads
    /// `TONIE_AUTH_TOKEN_LENGTH` bytes after `"BD "`.
    #[test]
    fn a_token_is_written_as_the_authorization_header() {
        let mut out = [0u8; 512];
        let token = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD,
            0xEE, 0xFF, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB,
            0xCC, 0xDD, 0xEE, 0xFF,
        ];
        let request = ContentRequest {
            uid: UID,
            route: Route::V1,
            etag: None,
            server: "box.lan:8080",
            from: None,
            auth: Some(&token),
        };
        let n = build_content_request(&request, &mut out).unwrap();
        let text = core::str::from_utf8(&out[..n]).unwrap();
        assert!(
            text.contains(
                "Authorization: BD 00112233445566778899AABBCCDDEEFF00112233445566778899AABBCCDDEEFF\r\n"
            ),
            "wrote: {text}"
        );
    }

    /// No token, no header: not an empty `Authorization: BD `.
    #[test]
    fn no_token_means_no_authorization_header() {
        let mut out = [0u8; 512];
        let request = ContentRequest {
            uid: UID,
            route: Route::V1,
            etag: None,
            server: "box.lan:8080",
            from: None,
            auth: None,
        };
        let n = build_content_request(&request, &mut out).unwrap();
        let text = core::str::from_utf8(&out[..n]).unwrap();
        assert!(!text.contains("Authorization"), "wrote: {text}");
    }
    /// No `Connection: close`: teddyCloud's `/content/` handler then omits
    /// `Content-Length`, which `begin_prepared` requires.
    #[test]
    fn a_path_request_asks_for_the_file_and_names_the_host() {
        let mut buf = [0u8; 256];
        let n = build_path_request(&mut buf, "/teddiebox.txt", "teddycloud.local", None).unwrap();
        assert_eq!(
            core::str::from_utf8(&buf[..n]).unwrap(),
            "GET /teddiebox.txt HTTP/1.1\r\n\
             Host: teddycloud.local\r\n\
             \r\n"
        );
    }

    #[test]
    fn a_resumed_path_request_carries_an_open_ended_range() {
        let mut buf = [0u8; 256];
        let n =
            build_path_request(&mut buf, "/teddiebox.bin", "teddycloud.local", Some(65536)).unwrap();
        assert_eq!(
            core::str::from_utf8(&buf[..n]).unwrap(),
            "GET /teddiebox.bin HTTP/1.1\r\n\
             Host: teddycloud.local\r\n\
             Range: bytes=65536-\r\n\
             \r\n"
        );
    }

    #[test]
    fn a_path_request_that_will_not_fit_the_buffer_is_refused() {
        let mut buf = [0u8; 8];
        assert_eq!(
            build_path_request(&mut buf, "/teddiebox.txt", "teddycloud.local", None),
            Err(CloudError::RequestTooLong)
        );
    }

    /// **Lower case, and it matters.** teddyCloud decides whether a tag is a
    /// "custom tonie" by comparing characters 10 to 15 of the identifier with
    /// `0304e0`, in lower case (`checkCustomTonie` in `handler_cloud.c`). An
    /// upper-case `E` fails the comparison, and the server marks the tag
    /// `nocloud` in its metadata. From then on every request for that figure
    /// gets a 404 with no upstream fetch, until someone clears the flag.
    #[test]
    fn the_identifier_is_written_in_lower_case() {
        let mut out = [0u8; 512];
        let request = ContentRequest {
            uid: [0xE0, 0x04, 0x03, 0x50, 0x50, 0x3F, 0x2E, 0x1D],
            route: Route::V2,
            etag: None,
            server: "box.lan:8080",
            from: None,
            auth: None,
        };
        let n = build_content_request(&request, &mut out).unwrap();
        let text = core::str::from_utf8(&out[..n]).unwrap();
        assert!(
            text.starts_with("GET /v2/content/1d2e3f50500304e0 "),
            "wrote: {}",
            text.lines().next().unwrap_or("")
        );
    }
}
