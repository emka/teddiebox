#![no_std]

//! Client for a LAN-local teddyCloud server.

pub mod client;
pub mod request;
pub mod response;

pub use client::{fetch, Outcome};
pub use request::{build_content_request, ETag};
pub use response::{parse_head, ResponseHead};

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
}
