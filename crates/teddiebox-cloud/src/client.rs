//! One request/response exchange over any byte transport.

use crate::{build_content_request, parse_head, CloudError, ETag};
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
    let mut request = [0u8; 512];
    let n = build_content_request(uid, etag, server, &mut request)?;
    transport
        .write_all(&request[..n])
        .map_err(|_| CloudError::Transport)?;
    transport.flush().map_err(|_| CloudError::Transport)?;

    let mut received = 0usize;
    loop {
        if received == buf.len() {
            break;
        }
        let n = transport
            .read(&mut buf[received..])
            .map_err(|_| CloudError::Transport)?;
        if n == 0 {
            break;
        }
        received += n;
    }

    let (head, body_at) = parse_head(&buf[..received])?;
    match head.status {
        200 => Ok(Outcome::Updated {
            etag: head.etag,
            content_length: head.content_length,
            body_at,
            received,
        }),
        304 => Ok(Outcome::Unchanged),
        404 => Ok(Outcome::NotFound),
        other => Err(CloudError::UnexpectedStatus(other)),
    }
}
