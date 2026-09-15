//! Just enough HTTP to answer two routes.
//!
//! Hand-rolled rather than a server crate because the surface is two paths and
//! one verb each — and because the box already hand-rolls its HTTP *client* in
//! `teddiebox-cloud`, so this is the shape the project already reads.

use crate::MAX_CONFIG;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    /// Anything else. Recognised rather than refused, so the caller answers a
    /// 404 instead of dropping the connection.
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestError {
    /// The headers have not all arrived. Read more and call again.
    Incomplete,
    Malformed,
    /// `Content-Length` exceeds [`MAX_CONFIG`] — refused before the body is
    /// read rather than after.
    TooLarge,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Request<'a> {
    pub method: Method,
    pub path: &'a str,
    pub content_length: usize,
    /// Bytes up to and including the blank line, so `buf[header_len..]` is as
    /// much of the body as has arrived.
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
    if content_length > MAX_CONFIG {
        return Err(RequestError::TooLarge);
    }

    Ok(Request {
        method,
        path,
        content_length,
        header_len,
    })
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
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

    /// The socket hands over whatever arrived. Headers split mid-way must read
    /// as "not yet", never as a malformed request.
    #[test]
    fn headers_still_arriving_are_incomplete_not_malformed() {
        assert_eq!(parse(b"GET / HTT").unwrap_err(), RequestError::Incomplete);
        assert_eq!(
            parse(b"GET / HTTP/1.1\r\nHost: x\r\n").unwrap_err(),
            RequestError::Incomplete
        );
    }

    /// Headers complete, body still coming: the request parses, and the caller
    /// compares `content_length` against what it has to decide whether to read
    /// more. This is the split-across-two-reads case.
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

    #[test]
    fn a_content_length_that_is_not_a_number_is_malformed() {
        let raw = b"POST /save HTTP/1.1\r\nContent-Length: yes\r\n\r\n";
        assert_eq!(parse(raw).unwrap_err(), RequestError::Malformed);
    }
}
