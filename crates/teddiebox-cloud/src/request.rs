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
    /// teddyCloud never checks this. It forwards it to the tonies cloud when
    /// it does not already hold the content, and that is what accepts or
    /// rejects it — so supplying it is the difference between a `403` and a
    /// story for any figure the server has not already got.
    ///
    /// `None` for content the server holds, which needs no token at all.
    pub auth: Option<&'a [u8; TOKEN_BYTES]>,
}

/// Length of a tag's authentication token: the whole of its user memory.
///
/// `TONIE_AUTH_TOKEN_LENGTH` in teddyCloud's `include/net_config.h`, and eight
/// four-byte ICODE SLIX-L blocks at the other end — the two agree, which is
/// what made "the token is the tag's memory" checkable rather than a guess.
pub const TOKEN_BYTES: usize = 32;

/// Which of teddyCloud's two content routes to ask.
///
/// [`Route::V2`] is the box's real endpoint and the default. [`Route::V1`]
/// exists because the teddyCloud this project talks to does not answer on
/// `/v2` at all — measured 2026-09-03, the connection is accepted and then
/// nothing comes back, with and without an `Authorization` header, until the
/// client times out. `/v1` returns the file. Upstream's source has `/v1` pass
/// `noPassword = TRUE`, which is the likely difference, but the hang itself is
/// unexplained and this is a way round it rather than a fix for it.
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
/// The path is `/v2/content/<ruid>` — teddyCloud's box-facing content route
/// (`handleCloudContentV2` in its `server.c`/`handler_cloud.c`), not the
/// similarly-named `/content/` which is that server's *web admin* API. Hitting
/// the admin route would be a silent wrong-endpoint bug: same shape of URL,
/// different handler.
///
/// `<ruid>` is the UID with its bytes reversed, not the UID itself: teddyCloud
/// recovers the UID by byte-swapping whatever comes after `/v2/content/`
/// (`handler_cloud.c`'s `bswap_64` on the parsed ruid), and builds that same
/// ruid from a UID on its own side by reversing the eight hex-pair bytes
/// (`handler.c`'s `getContentPathFromUID`). Sending the UID's own hex would
/// look plausible — same sixteen characters, same charset — while asking the
/// server for a different, generally nonexistent, tag.
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

    write!(buf, "GET {}", request.route.path()).map_err(|_| CloudError::RequestTooLong)?;
    for b in request.uid.iter().rev() {
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

    /// A UID whose reversed hex starts with a leading zero byte, so a naive
    /// `u64`-arithmetic reversal (bswap-then-format) that drops the leading
    /// zero digits would be caught: reversed correctly this is
    /// `0077665544332211`, not `77665544332211`.
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
    fn the_identifier_is_the_byte_reversed_uid_as_uppercase_hex() {
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

    /// The old builder copied through a fixed 512-byte string, so a request
    /// longer than that failed however much room the caller offered.
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
    /// `/v1` exists because `/v2` does not answer on the teddyCloud this
    /// project talks to: measured 2026-09-03, `/v2/content/<ruid>` accepts the
    /// connection and then hangs with zero bytes until the client gives up,
    /// with and without an Authorization header, while `/v1` returns the file.
    /// The route is selectable rather than swapped so that the default stays
    /// the box's real endpoint.
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

    /// And the default is unchanged, so selecting a route cannot silently
    /// move every other caller onto it.
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
    /// The header that lets teddyCloud fetch a figure it does not already
    /// hold. It never checks the token itself — it forwards it to the tonies
    /// cloud, which is what rejects a bad one — so the box supplying it is the
    /// difference between a `403` and a story.
    ///
    /// Upper-case hex, and all thirty-two bytes: `src/server.c` reads
    /// `TONIE_AUTH_TOKEN_LENGTH` of them after the literal `"BD "`.
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

    /// No tag on the plate, no header. An empty or absent token must not become
    /// `Authorization: BD ` with nothing after it, which is a malformed header
    /// rather than an absent one.
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
}
