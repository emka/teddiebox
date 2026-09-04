//! One request/response exchange over any byte transport.

use crate::{build_content_request, parse_head, CloudError, ContentRequest, ETag, Route};
use embedded_io::{Read, Write};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The cached copy is current. Nothing to do.
    Unchanged,
    /// New content is available; `body_at` is where it starts in the buffer.
    Updated {
        etag: Option<ETag>,
        content_length: Option<u32>,
        body_at: usize,
        received: usize,
    },
    /// The server has no content for this tag.
    NotFound,
}

/// Performs a conditional GET. `buf` receives the raw response.
pub fn fetch<T: Read + Write>(
    transport: &mut T,
    uid: [u8; 8],
    etag: Option<&ETag>,
    server: &str,
    buf: &mut [u8],
) -> Result<Outcome, CloudError> {
    let mut request_buf = [0u8; 512];
    let n = build_content_request(
        &ContentRequest {
            uid,
            route: Route::default(),
            etag,
            server,
            from: None,
        },
        &mut request_buf,
    )?;
    transport
        .write_all(&request_buf[..n])
        .map_err(|_| CloudError::Transport)?;
    transport.flush().map_err(|_| CloudError::Transport)?;

    // Read only until the head is complete. Draining to EOF instead would
    // hand control of when this returns to the peer, and a server honouring
    // keep-alive never hangs up — which would wedge playback, not just the
    // fetch.
    let mut received = 0usize;
    let (head, body_at) = loop {
        match parse_head(&buf[..received]) {
            Ok(parsed) => break parsed,
            Err(CloudError::MalformedResponse) if received < buf.len() => {}
            Err(e) => return Err(e),
        }
        let n = transport
            .read(&mut buf[received..])
            .map_err(|_| CloudError::Transport)?;
        if n == 0 {
            // Gone before even the head arrived.
            return Err(CloudError::MalformedResponse);
        }
        received += n;
    };

    match head.status {
        // These carry no body, so there is nothing further to wait for.
        304 => return Ok(Outcome::Unchanged),
        404 => return Ok(Outcome::NotFound),
        200 => {}
        other => return Err(CloudError::UnexpectedStatus(other)),
    }

    let length = head.content_length.ok_or(CloudError::LengthRequired)? as usize;
    let body_end = body_at
        .checked_add(length)
        .ok_or(CloudError::ResponseTooLong)?;
    if body_end > buf.len() {
        return Err(CloudError::ResponseTooLong);
    }

    while received < body_end {
        let n = transport
            .read(&mut buf[received..body_end])
            .map_err(|_| CloudError::Transport)?;
        if n == 0 {
            return Err(CloudError::BodyTruncated);
        }
        received += n;
    }

    Ok(Outcome::Updated {
        etag: head.etag,
        content_length: head.content_length,
        body_at,
        received,
    })
}
