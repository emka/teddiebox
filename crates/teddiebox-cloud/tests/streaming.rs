//! A 27 MB story does not fit in a buffer, so the body is streamed. These
//! tests check that the caller always learns where the bytes belong and when
//! the server stopped early.

use core::task::Poll;
use pollster::block_on;
use teddiebox_cloud::{
    begin, begin_prepared, build_path_request, probe_length, Begun, Body, CloudError,
    ContentRequest, ETag, Probed,
};

const UID: [u8; 8] = [1, 2, 3, 4, 5, 6, 7, 8];

/// A transport that returns a canned response in fixed-size pieces, and can
/// return Pending once partway through, like a real socket with no data yet.
struct Fake {
    response: Vec<u8>,
    read_at: usize,
    bite: usize,
    written: Vec<u8>,
    pend_after: Option<usize>,
    pended: bool,
}

impl Fake {
    fn new(response: &[u8]) -> Self {
        Self {
            response: response.to_vec(),
            read_at: 0,
            bite: usize::MAX,
            written: Vec::new(),
            pend_after: None,
            pended: false,
        }
    }

    fn in_bites_of(mut self, bite: usize) -> Self {
        self.bite = bite;
        self
    }

    fn pending_after(mut self, bytes: usize) -> Self {
        self.pend_after = Some(bytes);
        self
    }
}

impl embedded_io_async::ErrorType for Fake {
    type Error = embedded_io_async::ErrorKind;
}

impl embedded_io_async::Read for Fake {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        core::future::poll_fn(|cx| {
            if let Some(at) = self.pend_after {
                if self.read_at >= at && !self.pended {
                    self.pended = true;
                    // Wake immediately: this tests the client's handling of
                    // Pending, not the executor.
                    cx.waker().wake_by_ref();
                    return Poll::Pending;
                }
            }
            let remaining = self.response.len() - self.read_at;
            let n = remaining.min(buf.len()).min(self.bite);
            buf[..n].copy_from_slice(&self.response[self.read_at..self.read_at + n]);
            self.read_at += n;
            Poll::Ready(Ok(n))
        })
        .await
    }
}

impl embedded_io_async::Write for Fake {
    async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        self.written.extend_from_slice(buf);
        Ok(buf.len())
    }

    async fn flush(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

fn request(from: Option<u32>) -> ContentRequest<'static> {
    ContentRequest {
        uid: UID,
        route: teddiebox_cloud::Route::V2,
        auth: None,
        etag: None,
        server: "box.lan:8080",
        from,
    }
}

#[test]
fn a_body_larger_than_the_buffer_is_pumped_rather_than_refused() {
    // 200 bytes of body through a 64-byte buffer, which `fetch` would refuse
    // with ResponseTooLong. The buffer must still fit the 40-byte head.
    let body: Vec<u8> = (0..200u32).map(|n| n as u8).collect();
    let mut raw = Vec::from(*b"HTTP/1.1 200 OK\r\nContent-Length: 200\r\n\r\n");
    raw.extend_from_slice(&body);
    let mut t = Fake::new(&raw);
    let mut buf = [0u8; 64];

    let begun = block_on(begin(&mut t, &request(None), &mut buf)).unwrap();
    let Begun::Content {
        body_length,
        offset,
        prefix,
        ..
    } = begun
    else {
        panic!("expected content");
    };
    assert_eq!(body_length, 200);
    assert_eq!(offset, 0);

    let mut collected = Vec::from(&buf[prefix.clone()]);
    let mut stream = Body::new(body_length, prefix.len() as u32);
    while !stream.is_complete() {
        let n = block_on(stream.read(&mut t, &mut buf)).unwrap();
        collected.extend_from_slice(&buf[..n]);
    }
    assert_eq!(collected, body);
}

#[test]
fn a_partial_response_says_where_its_bytes_belong() {
    let mut raw = Vec::from(
        *b"HTTP/1.1 206 Partial Content\r\n\
Content-Range: bytes 8-11/12\r\n\
Content-Length: 4\r\n\r\n",
    );
    raw.extend_from_slice(b"TAIL");
    let mut t = Fake::new(&raw);
    let mut buf = [0u8; 256];

    let Begun::Content {
        offset,
        total,
        body_length,
        prefix,
        ..
    } = block_on(begin(&mut t, &request(Some(8)), &mut buf)).unwrap()
    else {
        panic!("expected content");
    };
    assert_eq!(offset, 8, "a resumed body starts where the range says");
    assert_eq!(total, Some(12));
    assert_eq!(body_length, 4);
    assert_eq!(&buf[prefix], b"TAIL");
}

/// The server ignored the Range and sent the whole file, so the bytes belong
/// at offset zero, not appended to the partial file.
#[test]
fn a_server_that_ignores_the_range_reports_an_offset_of_zero() {
    let mut raw = Vec::from(*b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\n");
    raw.extend_from_slice(b"HEAD");
    let mut t = Fake::new(&raw);
    let mut buf = [0u8; 256];

    let Begun::Content { offset, total, .. } =
        block_on(begin(&mut t, &request(Some(8)), &mut buf)).unwrap()
    else {
        panic!("expected content");
    };
    assert_eq!(offset, 0);
    assert_eq!(
        total,
        Some(4),
        "a 200 states the whole length in Content-Length"
    );
}

#[test]
fn a_head_that_arrives_in_pieces_is_still_parsed() {
    let mut raw = Vec::from(*b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\n");
    raw.extend_from_slice(b"DATA");
    let mut t = Fake::new(&raw).in_bites_of(3);
    let mut buf = [0u8; 256];

    let Begun::Content {
        body_length,
        prefix,
        ..
    } = block_on(begin(&mut t, &request(None), &mut buf)).unwrap()
    else {
        panic!("expected content");
    };
    assert_eq!(body_length, 4);
    let mut collected = Vec::from(&buf[prefix.clone()]);
    let mut stream = Body::new(body_length, prefix.len() as u32);
    while !stream.is_complete() {
        let n = block_on(stream.read(&mut t, &mut buf)).unwrap();
        collected.extend_from_slice(&buf[..n]);
    }
    assert_eq!(collected, b"DATA");
}

/// A keep-alive server can leave bytes in the buffer after the body's end,
/// here in the same read as the head. They are not content and must not be
/// written to the file.
#[test]
fn trailing_bytes_after_the_body_are_not_handed_to_the_caller() {
    let mut raw = Vec::from(*b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\n");
    raw.extend_from_slice(b"DATATRAILING");
    let mut t = Fake::new(&raw);
    let mut buf = [0u8; 256];

    let Begun::Content {
        body_length,
        prefix,
        ..
    } = block_on(begin(&mut t, &request(None), &mut buf)).unwrap()
    else {
        panic!("expected content");
    };

    let mut collected = Vec::from(&buf[prefix.clone()]);
    let mut stream = Body::new(body_length, prefix.len() as u32);
    while !stream.is_complete() {
        let n = block_on(stream.read(&mut t, &mut buf)).unwrap();
        collected.extend_from_slice(&buf[..n]);
    }
    assert_eq!(collected, b"DATA");
}

/// A socket with no data yet returns Pending. The client must not treat it as
/// the end of the stream.
#[test]
fn a_transport_that_pends_partway_does_not_end_the_download() {
    let mut raw = Vec::from(*b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\n\r\n");
    raw.extend_from_slice(b"ABCDEFGH");
    let mut t = Fake::new(&raw).in_bites_of(4).pending_after(20);
    let mut buf = [0u8; 64];

    let Begun::Content {
        body_length,
        prefix,
        ..
    } = block_on(begin(&mut t, &request(None), &mut buf)).unwrap()
    else {
        panic!("expected content");
    };

    let mut collected = Vec::from(&buf[prefix.clone()]);
    let mut body = Body::new(body_length, prefix.len() as u32);
    while !body.is_complete() {
        let n = block_on(body.read(&mut t, &mut buf)).unwrap();
        collected.extend_from_slice(&buf[..n]);
    }
    assert_eq!(collected, b"ABCDEFGH");
}

/// A server that promises 100 bytes and sends 4 must not look like a complete
/// file.
#[test]
fn a_peer_that_stops_early_is_an_error_rather_than_a_short_file() {
    let mut raw = Vec::from(*b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n");
    raw.extend_from_slice(b"DATA");
    let mut t = Fake::new(&raw);
    let mut buf = [0u8; 64];

    let Begun::Content {
        body_length,
        prefix,
        ..
    } = block_on(begin(&mut t, &request(None), &mut buf)).unwrap()
    else {
        panic!("expected content");
    };

    let mut body = Body::new(body_length, prefix.len() as u32);
    let mut err = None;
    while !body.is_complete() {
        match block_on(body.read(&mut t, &mut buf)) {
            Ok(_) => {}
            Err(e) => {
                err = Some(e);
                break;
            }
        }
    }
    assert_eq!(err, Some(CloudError::BodyTruncated));
}

#[test]
fn an_unchanged_file_reports_unchanged_and_reads_no_body() {
    let mut t = Fake::new(b"HTTP/1.1 304 Not Modified\r\nETag: \"v1\"\r\n\r\n");
    let etag = ETag::try_from("\"v1\"").unwrap();
    let mut buf = [0u8; 256];
    let request = ContentRequest {
        uid: UID,
        route: teddiebox_cloud::Route::V2,
        auth: None,
        etag: Some(&etag),
        server: "box.lan:8080",
        from: None,
    };
    assert_eq!(
        block_on(begin(&mut t, &request, &mut buf)).unwrap(),
        Begun::Unchanged
    );
    assert_eq!(
        t.read_at,
        t.response.len(),
        "a 304 carries no body, so nothing beyond the head should be read"
    );
}

#[test]
fn an_unknown_tag_reports_not_found() {
    let mut t = Fake::new(b"HTTP/1.1 404 Not Found\r\n\r\n");
    let mut buf = [0u8; 256];
    assert_eq!(
        block_on(begin(&mut t, &request(None), &mut buf)).unwrap(),
        Begun::NotFound
    );
}

#[test]
fn a_response_without_a_length_is_refused() {
    let mut t = Fake::new(b"HTTP/1.1 200 OK\r\n\r\nDATA");
    let mut buf = [0u8; 256];
    assert_eq!(
        block_on(begin(&mut t, &request(None), &mut buf)),
        Err(CloudError::LengthRequired)
    );
}

/// A head longer than the buffer can never be parsed, so it must fail rather
/// than loop forever.
#[test]
fn a_head_too_long_for_the_buffer_is_an_error_rather_than_a_spin() {
    let mut header = Vec::from(*b"HTTP/1.1 200 OK\r\n");
    for _ in 0..40 {
        header.extend_from_slice(b"X-Padding: 0123456789012345678901234567890123456789\r\n");
    }
    header.extend_from_slice(b"\r\n");
    let mut t = Fake::new(&header);
    let mut buf = [0u8; 128];
    assert_eq!(
        block_on(begin(&mut t, &request(None), &mut buf)),
        Err(CloudError::ResponseTooLong)
    );
}

/// The exact response teddyCloud sent to a one-byte probe, including the one
/// byte of body:
///
/// ```text
/// HTTP/1.1 206 Partial Content
/// Connection: keep-alive
/// Keep-Alive: timeout=300, max=4294967294
/// Accept-Ranges: bytes
/// Content-Type: application/octet-stream
/// Content-Range: bytes 1-1/37912939
/// Content-Length: 1
/// ```
#[test]
fn a_probe_reports_the_whole_file_length_from_one_byte() {
    let response = b"HTTP/1.1 206 Partial Content\r\n\
Connection: keep-alive\r\n\
Keep-Alive: timeout=300, max=4294967294\r\n\
Accept-Ranges: bytes\r\n\
Content-Type: application/octet-stream\r\n\
Content-Range: bytes 1-1/37912939\r\n\
Content-Length: 1\r\n\r\nX";
    let mut transport = Fake::new(response);
    let mut buf = [0u8; 512];

    let probed = block_on(probe_length(&mut transport, &request(None), &mut buf)).unwrap();

    assert_eq!(probed, Probed::Length(37_912_939));
    let sent = String::from_utf8(transport.written.clone()).unwrap();
    assert!(sent.contains("Range: bytes=1-1\r\n"), "sent: {sent}");
}

/// For a ruid the Tonies cloud does not know, teddyCloud passes on the
/// cloud's **410 Gone**. That is no reason to delete a story already on the
/// card.
#[test]
fn a_probe_for_an_unknown_figure_is_not_an_error() {
    let response = b"HTTP/1.1 410 Gone\r\nContent-Length: 39\r\n\r\n";
    let mut transport = Fake::new(response);
    let mut buf = [0u8; 512];

    let probed = block_on(probe_length(&mut transport, &request(None), &mut buf)).unwrap();

    assert_eq!(probed, Probed::NoContent);
}

/// A server that answers the probe with the whole file (teddyCloud does for
/// a range at offset zero) sends `Content-Length` and no `Content-Range`. That
/// length is the file's length.
#[test]
fn a_probe_answered_with_the_whole_file_still_reports_its_length() {
    let response = b"HTTP/1.1 200 OK\r\nContent-Length: 60975\r\n\r\n";
    let mut transport = Fake::new(response);
    let mut buf = [0u8; 512];

    let probed = block_on(probe_length(&mut transport, &request(None), &mut buf)).unwrap();

    assert_eq!(probed, Probed::Length(60_975));
}

/// A `304` has no length. A probe sends no conditional header, so it should
/// not get one, but if it does, guessing a length could wrongly mark a good
/// file stale.
#[test]
fn a_probe_that_learns_no_length_says_so() {
    let response = b"HTTP/1.1 304 Not Modified\r\n\r\n";
    let mut transport = Fake::new(response);
    let mut buf = [0u8; 512];

    let probed = block_on(probe_length(&mut transport, &request(None), &mut buf)).unwrap();

    assert_eq!(probed, Probed::Unstated);
}

/// The same `410 Gone` on the download path. It means "no story", not "the
/// server could not be reached".
#[test]
fn a_figure_the_cloud_never_heard_of_is_not_a_network_fault() {
    let response = b"HTTP/1.1 410 Gone\r\nContent-Length: 39\r\n\r\n";
    let mut transport = Fake::new(response);
    let mut buf = [0u8; 512];

    let begun = block_on(begin(&mut transport, &request(None), &mut buf)).unwrap();

    assert_eq!(begun, Begun::NotFound);
}

/// Used by OTA: a path request built by `build_path_request` goes through
/// `begin_prepared` and gives the same `Begun::Content` as `begin` does for a
/// content request, because both share the same code.
#[test]
fn begin_prepared_parses_a_path_response_the_same_way_begin_parses_a_content_response() {
    let raw = *b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nDATA";

    let mut path_bytes = [0u8; 512];
    let n = build_path_request(&mut path_bytes, "/teddiebox.bin", "box.lan:8080", None).unwrap();
    let mut path_transport = Fake::new(&raw);
    let mut path_buf = [0u8; 256];
    let path_begun = block_on(begin_prepared(
        &mut path_transport,
        &path_bytes[..n],
        &mut path_buf,
    ))
    .unwrap();

    let mut content_transport = Fake::new(&raw);
    let mut content_buf = [0u8; 256];
    let content_begun = block_on(begin(
        &mut content_transport,
        &request(None),
        &mut content_buf,
    ))
    .unwrap();

    assert_eq!(path_begun, content_begun);
    let Begun::Content {
        body_length,
        offset,
        prefix,
        ..
    } = path_begun
    else {
        panic!("expected content");
    };
    assert_eq!(body_length, 4);
    assert_eq!(offset, 0);
    assert_eq!(&path_buf[prefix], b"DATA");
}
