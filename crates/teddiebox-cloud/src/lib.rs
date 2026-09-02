#![no_std]

//! Client for a LAN-local teddyCloud server.

pub mod client;
pub mod request;
pub mod response;
pub mod stream;

pub use client::{fetch, Outcome};
pub use request::{build_content_request, ContentRequest, ETag, MAX_ETAG};
pub use response::{parse_head, ContentRange, ResponseHead};
pub use stream::{begin, Begun, Body};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloudError {
    RequestTooLong,
    Transport,
    MalformedResponse,
    UnexpectedStatus(u16),
    /// The peer closed before delivering the body it promised.
    BodyTruncated,
    /// The response will not fit the caller's buffer.
    ResponseTooLong,
    /// A response carrying content gave no `Content-Length`, so there is no
    /// way to know where the body ends without reading until the peer hangs
    /// up — which a keep-alive peer never does.
    LengthRequired,
    /// The body is chunked, and nothing here strips the chunk framing. Failing
    /// is the only honest answer: the alternative is handing chunk sizes to
    /// the Opus decoder as if they were audio.
    UnsupportedTransferEncoding,
}
