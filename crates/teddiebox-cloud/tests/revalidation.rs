//! The offline-first contract: revalidation must be cheap, and failure must
//! never be worse than not having asked.

use teddiebox_cloud::{fetch, CloudError, ETag, Outcome};

const UID: [u8; 8] = [1, 2, 3, 4, 5, 6, 7, 8];

/// A transport that replays a canned response and records what was written.
struct Fake {
    response: Vec<u8>,
    read_at: usize,
    written: Vec<u8>,
    fail_on_write: bool,
    /// Models a peer that keeps the connection open after answering, which a
    /// keep-alive server may legitimately do. A real socket would block here;
    /// the test panics instead, so "hangs forever" surfaces as a failure.
    block_after_response: bool,
}

impl Fake {
    fn new(response: &[u8]) -> Self {
        Self {
            response: response.to_vec(),
            read_at: 0,
            written: Vec::new(),
            fail_on_write: false,
            block_after_response: false,
        }
    }

    fn broken() -> Self {
        Self {
            response: Vec::new(),
            read_at: 0,
            written: Vec::new(),
            fail_on_write: true,
            block_after_response: false,
        }
    }
}

impl embedded_io::ErrorType for Fake {
    type Error = embedded_io::ErrorKind;
}

impl embedded_io::Read for Fake {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        if self.read_at == self.response.len() && self.block_after_response {
            panic!("read past the end of the response: this would block forever");
        }
        let remaining = self.response.len() - self.read_at;
        let n = remaining.min(buf.len());
        buf[..n].copy_from_slice(&self.response[self.read_at..self.read_at + n]);
        self.read_at += n;
        Ok(n)
    }
}

impl embedded_io::Write for Fake {
    fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        if self.fail_on_write {
            return Err(embedded_io::ErrorKind::Other);
        }
        self.written.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[test]
fn an_unchanged_file_costs_only_headers() {
    let mut t = Fake::new(b"HTTP/1.1 304 Not Modified\r\nETag: \"v1\"\r\n\r\n");
    let etag = ETag::try_from("\"v1\"").unwrap();
    let mut buf = [0u8; 1024];

    let outcome = fetch(&mut t, UID, Some(&etag), "box.lan:8080", &mut buf).unwrap();
    assert_eq!(outcome, Outcome::Unchanged);

    let sent = String::from_utf8(t.written.clone()).unwrap();
    assert!(sent.contains("If-None-Match: \"v1\""));
}

#[test]
fn a_changed_file_reports_the_new_content() {
    let mut t = Fake::new(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nETag: \"v2\"\r\n\r\nDATA");
    let etag = ETag::try_from("\"v1\"").unwrap();
    let mut buf = [0u8; 1024];

    match fetch(&mut t, UID, Some(&etag), "box.lan:8080", &mut buf).unwrap() {
        Outcome::Updated {
            etag,
            content_length,
            body_at,
            received,
        } => {
            assert_eq!(etag.as_deref(), Some("\"v2\""));
            assert_eq!(content_length, Some(4));
            assert_eq!(&buf[body_at..received], b"DATA");
        }
        other => panic!("expected Updated, got {other:?}"),
    }
}

#[test]
fn a_first_fetch_sends_no_conditional_header() {
    let mut t = Fake::new(b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\nX");
    let mut buf = [0u8; 1024];

    fetch(&mut t, UID, None, "box.lan:8080", &mut buf).unwrap();
    let sent = String::from_utf8(t.written.clone()).unwrap();
    assert!(!sent.contains("If-None-Match"));
}

#[test]
fn a_body_cut_short_is_an_error_rather_than_a_silent_truncation() {
    // Content-Length promises 100 bytes; the peer delivers 4 and closes.
    let mut t = Fake::new(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nDATA");
    let mut buf = [0u8; 1024];
    assert_eq!(
        fetch(&mut t, UID, None, "box.lan:8080", &mut buf),
        Err(CloudError::BodyTruncated),
        "reporting Updated here caches a truncated story permanently"
    );
}

#[test]
fn a_body_too_large_for_the_buffer_is_refused_up_front() {
    let mut t = Fake::new(b"HTTP/1.1 200 OK\r\nContent-Length: 4096\r\n\r\nDATA");
    let mut buf = [0u8; 128];
    assert_eq!(
        fetch(&mut t, UID, None, "box.lan:8080", &mut buf),
        Err(CloudError::ResponseTooLong)
    );
}

#[test]
fn a_response_without_a_length_is_refused_rather_than_read_to_exhaustion() {
    // No Content-Length means no way to know where the body ends, and reading
    // to EOF is what lets a keep-alive peer wedge the firmware.
    let mut t = Fake::new(b"HTTP/1.1 200 OK\r\n\r\nDATA");
    let mut buf = [0u8; 1024];
    assert_eq!(
        fetch(&mut t, UID, None, "box.lan:8080", &mut buf),
        Err(CloudError::LengthRequired)
    );
}

#[test]
fn revalidation_stops_reading_once_the_body_is_complete() {
    // The peer ignores Connection: close and holds the socket open, as any
    // HTTP/1.1 server may. Reading to EOF would never return.
    let mut t = Fake::new(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nDATA");
    t.block_after_response = true;
    let mut buf = [0u8; 1024];

    match fetch(&mut t, UID, None, "box.lan:8080", &mut buf).unwrap() {
        Outcome::Updated {
            body_at, received, ..
        } => assert_eq!(&buf[body_at..received], b"DATA"),
        other => panic!("expected Updated, got {other:?}"),
    }
}

#[test]
fn an_unchanged_file_does_not_wait_for_the_peer_to_hang_up() {
    let mut t = Fake::new(b"HTTP/1.1 304 Not Modified\r\nETag: \"v1\"\r\n\r\n");
    t.block_after_response = true;
    let etag = ETag::try_from("\"v1\"").unwrap();
    let mut buf = [0u8; 1024];

    assert_eq!(
        fetch(&mut t, UID, Some(&etag), "box.lan:8080", &mut buf).unwrap(),
        Outcome::Unchanged
    );
}

#[test]
fn an_unknown_tag_reports_not_found() {
    let mut t = Fake::new(b"HTTP/1.1 404 Not Found\r\n\r\n");
    let mut buf = [0u8; 1024];
    assert_eq!(
        fetch(&mut t, UID, None, "box.lan:8080", &mut buf).unwrap(),
        Outcome::NotFound
    );
}

#[test]
fn a_dead_network_is_an_error_the_caller_can_ignore() {
    let mut t = Fake::broken();
    let mut buf = [0u8; 1024];
    assert_eq!(
        fetch(&mut t, UID, None, "box.lan:8080", &mut buf),
        Err(CloudError::Transport)
    );
}

#[test]
fn an_unexpected_status_is_reported_with_its_code() {
    let mut t = Fake::new(b"HTTP/1.1 500 Internal Server Error\r\n\r\n");
    let mut buf = [0u8; 1024];
    assert_eq!(
        fetch(&mut t, UID, None, "box.lan:8080", &mut buf),
        Err(CloudError::UnexpectedStatus(500))
    );
}
