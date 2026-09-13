//! One download, driven asynchronously and pumped rather than buffered.
//!
//! [`crate::fetch`] reads a whole body into one buffer, which is right for a
//! revalidation and impossible for content: a Tonie is tens of megabytes and
//! the box has half of one. Here the head is parsed into the caller's buffer,
//! and the body is handed back a bite at a time.
//!
//! Nothing here knows what the bytes are for. `build_content_request` and
//! `parse_head` do the thinking; this file only moves bytes and counts them.

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
/// Draining to EOF instead would hand the peer control of when this returns,
/// and a keep-alive server never hangs up — which would wedge playback rather
/// than merely the fetch. Answers the head, where the body starts in `buf`,
/// and how much arrived in total, since the bytes past the head are body that
/// has already been paid for.
async fn read_head<T: Read + Write>(
    transport: &mut T,
    buf: &mut [u8],
) -> Result<(crate::ResponseHead, usize, usize), CloudError> {
    let mut received = 0usize;
    loop {
        match parse_head(&buf[..received]) {
            Ok((head, body_at)) => return Ok((head, body_at, received)),
            Err(CloudError::MalformedResponse) if received < buf.len() => {}
            // A head that will not fit cannot be parsed however long we wait.
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
    /// `410 Gone` as well as `404`: measured against `teddycloud.local` on
    /// 2026-09-13, a ruid the cloud has never heard of comes back `410` from
    /// the upstream proxy rather than `404` from teddyCloud. Either way it is
    /// an answer about the *server*, and no reason to discard a story already
    /// sitting on the card.
    NoContent,
    /// The server answered, and its answer contains no length.
    ///
    /// Kept apart from a transport failure on purpose. Both mean "do not act",
    /// but only one of them is worth saying out loud at a bench.
    Unstated,
}

/// Asks how long a file is now, without downloading it.
///
/// Reads exactly as far as the head. The one byte of body a `206` carries is
/// left in `buf` and ignored — it is a byte of story, not an answer.
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
        // A `206` states the whole length in its `Content-Range`; a `200` is
        // the whole file, so its `Content-Length` is the same number. This
        // server answers a zero-offset range with exactly that, which is why
        // the case is real rather than defensive.
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
    transport
        .write_all(&request_bytes[..n])
        .await
        .map_err(|_| CloudError::Transport)?;
    transport.flush().await.map_err(|_| CloudError::Transport)?;

    let (head, body_at, received) = read_head(transport, buf).await?;

    match head.status {
        304 => return Ok(Begun::Unchanged),
        // `410` as well as `404`. Measured against `teddycloud.local` on
        // 2026-09-13: a ruid the cloud has never heard of comes back `410
        // Gone` from the upstream proxy rather than `404` from teddyCloud.
        // Reported as an unexpected status it reached the box as "the server
        // could not be reached", which sent a child to look at a network that
        // was working perfectly.
        404 | 410 => return Ok(Begun::NotFound),
        200 | 206 => {}
        other => return Err(CloudError::UnexpectedStatus(other)),
    }

    let body_length = head.content_length.ok_or(CloudError::LengthRequired)?;

    // A 200 answering a range request means the server declined it and sent
    // the file from the start. Saying so as `offset: 0` is what stops a caller
    // appending the beginning of the story to the middle of a partial file.
    let (offset, total) = match head.content_range {
        Some(range) => (range.first, range.total),
        None => (0, Some(body_length)),
    };

    // `received` may hold bytes beyond the body — a pipelining or keep-alive
    // peer, or any trailing garbage — and those are never body. Clamping
    // here, not just in `Body::read`, is what stops them reaching the
    // caller as if they were content. `saturating_add` rather than a plain
    // `+`: `body_at + body_length` must not wrap `usize` on a 32-bit target,
    // where `usize` is the same width as `u32`; the `.min(received)` after
    // it means a saturated `usize::MAX` still clamps down to `received`.
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
/// Counting is the whole job: a peer that closes early otherwise looks exactly
/// like a file that ended, and the difference is a story that stops in the
/// middle forever.
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
    /// Never reads past the end of the body, so a peer holding the socket open
    /// afterwards cannot wedge the caller.
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
