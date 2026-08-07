#![no_std]

//! Client for a LAN-local teddyCloud server.

pub mod request;
pub mod response;

pub use request::{build_content_request, ETag};
pub use response::{parse_head, ResponseHead};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloudError {
    RequestTooLong,
    Transport,
    MalformedResponse,
    UnexpectedStatus(u16),
}
