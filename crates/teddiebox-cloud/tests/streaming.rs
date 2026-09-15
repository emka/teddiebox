//! A 27 MB story does not fit in a buffer, so the body is pumped rather than
//! collected. These tests hold the contract that makes that safe: the caller
//! always learns where the bytes belong and always learns when the peer
//! stopped early.

use core::task::Poll;
use pollster::block_on;
use teddiebox_cloud::{
    begin, begin_prepared, build_path_request, probe_length, Begun, Body, CloudError,
    ContentRequest, ETag, Probed,
};

const UID: [u8; 8] = [1, 2, 3, 4, 5, 6, 7, 8];

/// A transport that hands back a canned response in fixed-size bites, and can
/// pend once partway through to model a real socket that has nothing yet.
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
                    // Wake immediately: the point is to prove the client
                    // survives a Pending, not to test the executor.
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
    // The whole point: 200 bytes of body through a 64-byte buffer. `fetch`
    // answers ResponseTooLong here, which for a real Tonie is every time.
    // The buffer must still hold the head — 40 bytes of it — or the failure
    // under test is the wrong one.
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

/// The server ignored the Range and sent the whole file. The bytes belong at
/// zero, and a caller that appended them to a partial file would splice the
/// beginning of the story into its middle.
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

/// A pipelining or keep-alive peer can leave bytes in the buffer past the
/// body's declared end — here, garbage that arrived in the same read as the
/// head. Those bytes are not content, and a caller that wrote them to the
/// file would corrupt it without any error to show for it.
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

/// A socket with nothing to give yet returns Pending. The client must survive
/// it rather than treat it as end of stream.
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

/// The failure the sidecar exists to prevent, caught one layer earlier: a peer
/// that promises 100 bytes and delivers 4 must not look like a complete file.
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

/// A head longer than the buffer cannot be parsed and must not spin. Failing
/// says so; looping forever is what a naive "read until parsed" does.
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

/// The exact head `teddycloud.local` answered a one-byte probe with on
/// 2026-09-13, byte for byte, including the one byte of body it carried:
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

/// What the same server answers for a ruid it has never heard of: **410 Gone**,
/// from the upstream proxy rather than from teddyCloud. Measured the same day.
/// A figure the cloud has no story for is not a reason to discard a story that
/// is sitting on the card.
#[test]
fn a_probe_for_an_unknown_figure_is_not_an_error() {
    let response = b"HTTP/1.1 410 Gone\r\nContent-Length: 39\r\n\r\n";
    let mut transport = Fake::new(response);
    let mut buf = [0u8; 512];

    let probed = block_on(probe_length(&mut transport, &request(None), &mut buf)).unwrap();

    assert_eq!(probed, Probed::NoContent);
}

/// A server that answers the probe with the whole file — which this one does
/// for a range at offset zero — states a `Content-Length` and no
/// `Content-Range`. That length is the file's length, and saying so is better
/// than reading 37 MB to find out.
#[test]
fn a_probe_answered_with_the_whole_file_still_reports_its_length() {
    let response = b"HTTP/1.1 200 OK\r\nContent-Length: 60975\r\n\r\n";
    let mut transport = Fake::new(response);
    let mut buf = [0u8; 512];

    let probed = block_on(probe_length(&mut transport, &request(None), &mut buf)).unwrap();

    assert_eq!(probed, Probed::Length(60_975));
}

/// A `304` carries no length. It cannot arrive in answer to a probe, which
/// sends no conditional header — but a server is free to be wrong, and
/// guessing a length here would mark a perfectly good file stale.
#[test]
fn a_probe_that_learns_no_length_says_so() {
    let response = b"HTTP/1.1 304 Not Modified\r\n\r\n";
    let mut transport = Fake::new(response);
    let mut buf = [0u8; 512];

    let probed = block_on(probe_length(&mut transport, &request(None), &mut buf)).unwrap();

    assert_eq!(probed, Probed::Unstated);
}

/// The same `410 Gone` the probe learned about, on the fetch path.
///
/// Measured against `teddycloud.local` on 2026-09-13: a ruid the cloud has never
/// heard of comes back `410` from the upstream proxy, not `404` from
/// teddyCloud. Reported as an unexpected status, it reached the box's
/// vocabulary as "the server could not be reached" — so a figure with no story
/// anywhere told a child to go and look at their network.
#[test]
fn a_figure_the_cloud_never_heard_of_is_not_a_network_fault() {
    let response = b"HTTP/1.1 410 Gone\r\nContent-Length: 39\r\n\r\n";
    let mut transport = Fake::new(response);
    let mut buf = [0u8; 512];

    let begun = block_on(begin(&mut transport, &request(None), &mut buf)).unwrap();

    assert_eq!(begun, Begun::NotFound);
}

/// The seam OTA needs: a path request built by `build_path_request` goes
/// through `begin_prepared` and comes out the same `Begun::Content` shape
/// that `begin` produces for an equivalent content request — same status
/// classification, same `body_end` clamp, because both run through the one
/// copy of that logic rather than a duplicate.
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
