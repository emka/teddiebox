#![no_std]

//! What an over-the-air update decides, separated from how its bytes move.
//!
//! Nothing here does I/O. The firmware supplies the socket, the flash region
//! and the reboot; this crate owns the questions that have right and wrong
//! answers — whether a manifest is well formed, whether its version differs
//! from ours, whether the digest matched, which sector a write lands in — so
//! that all of them can be tested without a box.

mod decide;
mod digest;
mod manifest;

pub use decide::{decide, Decision, Refusal};
pub use manifest::{Manifest, FILENAME, MAX_IMAGE_PATH, MAX_MANIFEST, MAX_VERSION};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OtaError {
    /// A line is not `key = value` and is not blank or a comment.
    MalformedLine,
    MissingVersion,
    MissingSha256,
    MissingLength,
    MissingImage,
    /// A value does not fit the fixed buffer that holds it.
    ValueTooLong,
    /// The bytes are not UTF-8, so they are not this file.
    NotText,
    /// The read filled its buffer, so the file may have been cut short.
    Truncated,
    /// A digest was not exactly 64 hex characters.
    MalformedDigest,
}
