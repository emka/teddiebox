//! One asynchronous download, streamed rather than buffered.
//!
//! [`crate::fetch`] reads a whole body into one buffer, which cannot work for
//! a Tonie of tens of megabytes on a box with well under one megabyte of RAM.
//! Here the head is parsed into the caller's buffer, and the body is returned
//! piece by piece.
//!
//! This file only moves and counts bytes; `build_content_request` and
//! `parse_head` make the decisions.

use crate::{
    build_content_request, build_length_probe, parse_head, CloudError, ContentRequest, ETag,
};
use embedded_io_async::{Read, Write};

/// What a response turned out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Begun {
    /// The cached copy is current. No body follows.
    Unchanged,
    /// The server has no content for this tag. No body follows.
    NotFound,
    /// A body follows, and these are its coordinates.
    Content {
        etag: Option<ETag>,
        /// Length of *this* body — not of the file, which differs on a 206.
        body_length: u32,
        /// Where this body's first byte belongs within the whole file. Zero
        /// unless the server honoured a range.
        offset: u32,
        /// Length of the whole file, when the server said.
        total: Option<u32>,
        /// Body bytes that arrived alongside the head, as a range in `buf`.
        prefix: core::ops::Range<usize>,
    },
}

/// Reads until the response head is complete, and no further.
///
/// Reading until the server closes would hang with a keep-alive server. Returns
/// the head, where the body starts in `buf`, and how many bytes arrived in
/// total (bytes after the head are already part of the body).
async fn read_head<T: Read + Write>(
    transport: &mut T,
    buf: &mut [u8],
) -> Result<(crate::ResponseHead, usize, usize), CloudError> {
    let mut received = 0usize;
    loop {
        match parse_head(&buf[..received]) {
            Ok((head, body_at)) => return Ok((head, body_at, received)),
            Err(CloudError::MalformedResponse) if received < buf.len() => {}
            // A head that does not fit the buffer can never be parsed.
            Err(CloudError::MalformedResponse) => return Err(CloudError::ResponseTooLong),
            Err(e) => return Err(e),
        }
        let n = transport
            .read(&mut buf[received..])
            .await
            .map_err(|_| CloudError::Transport)?;
        if n == 0 {
            return Err(CloudError::MalformedResponse);
        }
        received += n;
    }
}

/// What a length probe learned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Probed {
    /// How long the whole file is now.
    Length(u32),
    /// The server has no story for this figure.
    ///
    /// `410 Gone` as well as `404`: for a ruid the Tonies cloud does not know,
    /// teddyCloud passes on the cloud's `410`. Either way, this is no reason
    /// to delete a story already on the card.
    NoContent,
    /// The server answered, and its answer contains no length.
    ///
    /// Separate from a transport failure. Both mean "do nothing", but they
    /// are logged differently.
    Unstated,
}

/// Asks how long a file is now, without downloading it.
///
/// Reads only the head. The one byte of body a `206` carries is left in `buf`
/// and ignored.
pub async fn probe_length<T: Read + Write>(
    transport: &mut T,
    request: &ContentRequest<'_>,
    buf: &mut [u8],
) -> Result<Probed, CloudError> {
    let mut request_bytes = [0u8; 512];
    let n = build_length_probe(request, &mut request_bytes)?;
    transport
        .write_all(&request_bytes[..n])
        .await
        .map_err(|_| CloudError::Transport)?;
    transport.flush().await.map_err(|_| CloudError::Transport)?;

    let (head, _, _) = read_head(transport, buf).await?;
    Ok(match head.status {
        404 | 410 => Probed::NoContent,
        // A `206` gives the whole length in its `Content-Range`; a `200` is
        // the whole file, so its `Content-Length` is the length. teddyCloud
        // really does answer some ranges with `200`.
        206 => match head.content_range.and_then(|range| range.total) {
            Some(total) => Probed::Length(total),
            None => Probed::Unstated,
        },
        200 => match head.content_length {
            Some(length) => Probed::Length(length),
            None => Probed::Unstated,
        },
        _ => Probed::Unstated,
    })
}

/// Sends the request and parses the response head.
///
/// On return, `buf` holds the head followed by however much of the body came
/// with it; `Begun::Content::prefix` is that much.
pub async fn begin<T: Read + Write>(
    transport: &mut T,
    request: &ContentRequest<'_>,
    buf: &mut [u8],
) -> Result<Begun, CloudError> {
    let mut request_bytes = [0u8; 512];
    let n = build_content_request(request, &mut request_bytes)?;
    begin_prepared(transport, &request_bytes[..n], buf).await
}

/// Sends already-built request bytes and parses the response head.
///
/// Like [`begin`], for requests that are not a [`ContentRequest`], such as a
/// path fetch (`build_path_request`). Both share the status handling below.
pub async fn begin_prepared<T: Read + Write>(
    transport: &mut T,
    request_bytes: &[u8],
    buf: &mut [u8],
) -> Result<Begun, CloudError> {
    transport
        .write_all(request_bytes)
        .await
        .map_err(|_| CloudError::Transport)?;
    transport.flush().await.map_err(|_| CloudError::Transport)?;

    let (head, body_at, received) = read_head(transport, buf).await?;

    match head.status {
        304 => return Ok(Begun::Unchanged),
        // `410` as well as `404`: for a ruid the Tonies cloud does not know,
        // teddyCloud passes on the cloud's `410 Gone`. Treating it as an
        // unexpected status would report a network problem that does not
        // exist.
        404 | 410 => return Ok(Begun::NotFound),
        200 | 206 => {}
        other => return Err(CloudError::UnexpectedStatus(other)),
    }

    let body_length = head.content_length.ok_or(CloudError::LengthRequired)?;

    // A 200 to a range request means the server sent the whole file from the
    // start. `offset: 0` stops the caller appending it to a partial file.
    let (offset, total) = match head.content_range {
        Some(range) => (range.first, range.total),
        None => (0, Some(body_length)),
    };

    // `received` may hold bytes after the body (from a keep-alive peer, or
    // garbage). Clamp here so they never reach the caller as content.
    // `saturating_add` because `body_at + body_length` could overflow on a
    // 32-bit target; `.min(received)` then clamps it.
    let body_end = body_at.saturating_add(body_length as usize).min(received);

    Ok(Begun::Content {
        etag: head.etag,
        body_length,
        offset,
        total,
        prefix: body_at..body_end,
    })
}

/// The remainder of a body, counted down.
///
/// Counting bytes is what tells a connection that closed early from a file
/// that ended.
pub struct Body {
    remaining: u32,
}

impl Body {
    /// `already_received` is the prefix that arrived with the head.
    pub fn new(body_length: u32, already_received: u32) -> Self {
        Self {
            remaining: body_length.saturating_sub(already_received),
        }
    }

    pub fn is_complete(&self) -> bool {
        self.remaining == 0
    }

    pub fn remaining(&self) -> u32 {
        self.remaining
    }

    /// Reads the next bite of the body into `buf`.
    ///
    /// Never reads past the end of the body, so a server that keeps the
    /// connection open cannot make the caller hang.
    pub async fn read<T: Read>(
        &mut self,
        transport: &mut T,
        buf: &mut [u8],
    ) -> Result<usize, CloudError> {
        if self.remaining == 0 {
            return Ok(0);
        }
        let want = (self.remaining as usize).min(buf.len());
        let n = transport
            .read(&mut buf[..want])
            .await
            .map_err(|_| CloudError::Transport)?;
        if n == 0 {
            return Err(CloudError::BodyTruncated);
        }
        self.remaining -= n as u32;
        Ok(n)
    }
}
