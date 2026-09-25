#![no_std]

//! Client for a LAN-local teddyCloud server.

pub mod client;
pub mod request;
pub mod response;
pub mod stream;

pub use client::{fetch, Outcome};
pub use request::{
    build_content_request, build_length_probe, build_path_request, parse_etag, ContentRequest,
    ETag, Route, MAX_ETAG,
};
pub use response::{parse_head, ContentRange, ResponseHead};
pub use stream::{begin, begin_prepared, probe_length, Begun, Body, Probed};

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
    /// A response with content has no `Content-Length`, so the body's end is
    /// unknown. A keep-alive server never closes the connection to mark it.
    LengthRequired,
    /// The body is chunked, and nothing here removes the chunk framing.
    /// Without this error, chunk sizes would reach the Opus decoder as audio.
    UnsupportedTransferEncoding,
}
