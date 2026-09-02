//! One download, driven asynchronously and pumped rather than buffered.
//!
//! [`crate::fetch`] reads a whole body into one buffer, which is right for a
//! revalidation and impossible for content: a Tonie is tens of megabytes and
//! the box has half of one. Here the head is parsed into the caller's buffer,
//! and the body is handed back a bite at a time.
//!
//! Nothing here knows what the bytes are for. `build_content_request` and
//! `parse_head` do the thinking; this file only moves bytes and counts them.

use crate::{build_content_request, parse_head, CloudError, ContentRequest, ETag};
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

    // Read only until the head is complete. Draining to EOF instead would hand
    // the peer control of when this returns, and a keep-alive server never
    // hangs up.
    let mut received = 0usize;
    let (head, body_at) = loop {
        match parse_head(&buf[..received]) {
            Ok(parsed) => break parsed,
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
    };

    match head.status {
        304 => return Ok(Begun::Unchanged),
        404 => return Ok(Begun::NotFound),
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

    Ok(Begun::Content {
        etag: head.etag,
        body_length,
        offset,
        total,
        prefix: body_at..received,
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
