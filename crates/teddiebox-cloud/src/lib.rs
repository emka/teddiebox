#![no_std]

//! Client for a LAN-local teddyCloud server.

pub mod request;

pub use request::{build_content_request, ETag};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloudError {
    RequestTooLong,
    Transport,
    MalformedResponse,
    UnexpectedStatus(u16),
}
