//! Just enough HTTP to answer two routes.
//!
//! Written by hand rather than with a server crate, because there are only
//! two routes, and the HTTP client in `teddiebox-cloud` is written the same
//! way.

use crate::MAX_BODY;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    /// Anything else, so the caller can answer 404 instead of dropping the
    /// connection.
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestError {
    /// The headers have not all arrived. Read more and call again.
    Incomplete,
    Malformed,
    /// `Content-Length` exceeds [`MAX_BODY`], so the body is not read.
    ///
    /// This is about the *request*, not the file inside it. A file that is too
    /// long is refused later by `form::field`, with a clearer message.
    TooLarge,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Request<'a> {
    pub method: Method,
    pub path: &'a str,
    pub content_length: usize,
    /// Bytes up to and including the blank line, so `buf[header_len..]` is
    /// the part of the body that has arrived.
    pub header_len: usize,
}

pub fn parse(buf: &[u8]) -> Result<Request<'_>, RequestError> {
    let end = find(buf, b"\r\n\r\n").ok_or(RequestError::Incomplete)?;
    let header_len = end + 4;
    let head = core::str::from_utf8(&buf[..end]).map_err(|_| RequestError::Malformed)?;

    let mut lines = head.split("\r\n");
    let mut request_line = lines.next().ok_or(RequestError::Malformed)?.split(' ');
    let method = match request_line.next().ok_or(RequestError::Malformed)? {
        "GET" => Method::Get,
        "POST" => Method::Post,
        _ => Method::Other,
    };
    let path = request_line.next().ok_or(RequestError::Malformed)?;

    let mut content_length = 0;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("content-length") {
            content_length = value
                .trim()
                .parse::<usize>()
                .map_err(|_| RequestError::Malformed)?;
        }
    }
    if content_length > MAX_BODY {
        return Err(RequestError::TooLarge);
    }

    Ok(Request {
        method,
        path,
        content_length,
        header_len,
    })
}

/// Whether `buffer` holds a whole request: the headers, and as much body as
/// `Content-Length` promises.
///
/// Returns `Ok(false)` rather than the parsed [`Request`], because the
/// caller needs the buffer back mutably to read more while the answer is
/// `false`.
pub fn is_complete(buffer: &[u8]) -> Result<bool, RequestError> {
    match parse(buffer) {
        Ok(request) => Ok(buffer.len() - request.header_len >= request.content_length),
        Err(RequestError::Incomplete) => Ok(false),
        Err(other) => Err(other),
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    NotFound,
    BadRequest,
    TooLarge,
}

impl Status {
    fn line(self) -> &'static str {
        match self {
            Status::Ok => "200 OK",
            Status::NotFound => "404 Not Found",
            Status::BadRequest => "400 Bad Request",
            Status::TooLarge => "413 Content Too Large",
        }
    }
}

/// Builds the response head.
///
/// Separate from the body, because `page` streams the body in pieces and
/// never builds it in one buffer.
///
/// `Connection: close`, because the portal has only one socket and answers
/// one request per connection.
pub fn head(status: Status, content_length: usize) -> heapless::Vec<u8, 192> {
    use core::fmt::Write;

    let mut out = heapless::String::<192>::new();
    // The fixed text is at most 111 bytes, plus up to 20 digits for a
    // `usize`: 131. 192 leaves some room.
    let _ = write!(
        out,
        "HTTP/1.1 {}\r\n\
Content-Type: text/html; charset=utf-8\r\n\
Content-Length: {content_length}\r\n\
Connection: close\r\n\r\n",
        status.line()
    );
    heapless::Vec::from_slice(out.as_bytes()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    const GET: &[u8] = b"GET / HTTP/1.1\r\nHost: 192.168.4.1\r\n\r\n";

    #[test]
    fn a_get_is_parsed() {
        let r = parse(GET).unwrap();
        assert_eq!(r.method, Method::Get);
        assert_eq!(r.path, "/");
        assert_eq!(r.content_length, 0);
        assert_eq!(r.header_len, GET.len());
    }

    #[test]
    fn a_post_carries_its_length() {
        let raw = b"POST /save HTTP/1.1\r\nHost: x\r\nContent-Length: 12\r\n\r\nconfig=ssid=";
        let r = parse(raw).unwrap();
        assert_eq!(r.method, Method::Post);
        assert_eq!(r.path, "/save");
        assert_eq!(r.content_length, 12);
        assert_eq!(r.header_len, raw.len() - 12); // b"config=ssid=" is 12 bytes
    }

    #[test]
    fn a_header_name_is_matched_without_regard_to_case() {
        let raw = b"POST /save HTTP/1.1\r\ncontent-length: 5\r\n\r\nabcde";
        assert_eq!(parse(raw).unwrap().content_length, 5);
    }

    /// The socket returns whatever has arrived. Headers split in the middle
    /// mean "not yet", not a malformed request.
    #[test]
    fn headers_still_arriving_are_incomplete_not_malformed() {
        assert_eq!(parse(b"GET / HTT").unwrap_err(), RequestError::Incomplete);
        assert_eq!(
            parse(b"GET / HTTP/1.1\r\nHost: x\r\n").unwrap_err(),
            RequestError::Incomplete
        );
    }

    /// Headers complete, body still arriving: the request parses, and the
    /// caller compares `content_length` with what it has to decide whether to
    /// read more.
    #[test]
    fn a_body_still_arriving_parses_so_the_caller_can_wait_for_it() {
        let raw = b"POST /save HTTP/1.1\r\nContent-Length: 20\r\n\r\nconfig=";
        let r = parse(raw).unwrap();
        assert_eq!(r.content_length, 20);
        assert_eq!(raw.len() - r.header_len, 7);
    }

    #[test]
    fn an_unsupported_method_is_recognised_not_refused() {
        assert_eq!(
            parse(b"PUT / HTTP/1.1\r\n\r\n").unwrap().method,
            Method::Other
        );
    }

    #[test]
    fn a_request_line_without_a_path_is_malformed() {
        assert_eq!(parse(b"GET\r\n\r\n").unwrap_err(), RequestError::Malformed);
    }

    #[test]
    fn a_body_larger_than_the_cap_is_refused_before_it_is_read() {
        let raw = b"POST /save HTTP/1.1\r\nContent-Length: 99999\r\n\r\n";
        assert_eq!(parse(raw).unwrap_err(), RequestError::TooLarge);
    }

    /// The limit applies to the encoded body, not the file. Written as
    /// literals rather than `MAX_BODY`, so changing the constant fails these
    /// tests.
    #[test]
    fn a_body_of_exactly_the_cap_is_accepted() {
        assert_eq!(MAX_BODY, 3088);
        let raw = b"POST /save HTTP/1.1\r\nContent-Length: 3088\r\n\r\n";
        assert_eq!(parse(raw).unwrap().content_length, 3088);
    }

    #[test]
    fn one_byte_over_the_cap_is_refused() {
        let raw = b"POST /save HTTP/1.1\r\nContent-Length: 3089\r\n\r\n";
        assert_eq!(parse(raw).unwrap_err(), RequestError::TooLarge);
    }

    /// A 1024-byte file encodes to more than 1024 bytes, and must still be
    /// accepted.
    #[test]
    fn a_form_encoded_full_size_config_is_no_longer_refused() {
        // 1024 file bytes at 1.35, the ratio for a realistic config. Most of
        // it is line endings: a textarea sends CRLF, encoded as `%0D%0A`.
        let raw = b"POST /save HTTP/1.1\r\nContent-Length: 1382\r\n\r\n";
        assert_eq!(parse(raw).unwrap().content_length, 1382);
    }

    #[test]
    fn a_content_length_that_is_not_a_number_is_malformed() {
        let raw = b"POST /save HTTP/1.1\r\nContent-Length: yes\r\n\r\n";
        assert_eq!(parse(raw).unwrap_err(), RequestError::Malformed);
    }

    #[test]
    fn an_ok_head_is_exactly_these_bytes() {
        assert_eq!(
            &head(Status::Ok, 42)[..],
            b"HTTP/1.1 200 OK\r\n\
Content-Type: text/html; charset=utf-8\r\n\
Content-Length: 42\r\n\
Connection: close\r\n\r\n"
        );
    }

    #[test]
    fn a_not_found_head_is_exactly_these_bytes() {
        assert_eq!(
            &head(Status::NotFound, 0)[..],
            b"HTTP/1.1 404 Not Found\r\n\
Content-Type: text/html; charset=utf-8\r\n\
Content-Length: 0\r\n\
Connection: close\r\n\r\n"
        );
    }

    #[test]
    fn a_bad_request_head_is_exactly_these_bytes() {
        assert_eq!(
            &head(Status::BadRequest, 7)[..],
            b"HTTP/1.1 400 Bad Request\r\n\
Content-Type: text/html; charset=utf-8\r\n\
Content-Length: 7\r\n\
Connection: close\r\n\r\n"
        );
    }

    #[test]
    fn a_too_large_head_is_exactly_these_bytes() {
        assert_eq!(
            &head(Status::TooLarge, 9)[..],
            b"HTTP/1.1 413 Content Too Large\r\n\
Content-Type: text/html; charset=utf-8\r\n\
Content-Length: 9\r\n\
Connection: close\r\n\r\n"
        );
    }

    #[test]
    fn the_largest_length_still_fits_the_head_buffer() {
        let built = head(Status::TooLarge, usize::MAX);
        assert!(built.ends_with(b"\r\n\r\n"));
    }

    #[test]
    fn headers_still_arriving_are_not_complete() {
        assert_eq!(is_complete(b"GET / HTT"), Ok(false));
    }

    #[test]
    fn headers_done_but_body_still_arriving_is_not_complete() {
        let raw = b"POST /save HTTP/1.1\r\nContent-Length: 20\r\n\r\nconfig=";
        assert_eq!(is_complete(raw), Ok(false));
    }

    #[test]
    fn headers_and_the_whole_body_are_complete() {
        let raw = b"POST /save HTTP/1.1\r\nContent-Length: 12\r\n\r\nconfig=ssid=";
        assert_eq!(is_complete(raw), Ok(true));
    }

    #[test]
    fn a_request_with_no_body_is_complete_at_the_blank_line() {
        assert_eq!(is_complete(GET), Ok(true));
    }

    #[test]
    fn a_malformed_request_is_an_error_not_incomplete() {
        assert_eq!(is_complete(b"GET\r\n\r\n"), Err(RequestError::Malformed));
    }
}
