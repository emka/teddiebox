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
mod image;
mod manifest;
mod sink;

pub use decide::{decide, Decision, Refusal};
pub use image::{image_version, may_activate};
pub use manifest::{Manifest, FILENAME, MAX_IMAGE_PATH, MAX_MANIFEST, MAX_VERSION};
pub use sink::{FlashRegionLike, Sectors, SinkError, SECTOR};

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
    /// The bytes handed to `image_version` are too short to hold an ESP-IDF
    /// application descriptor, or the magic word at its start is wrong.
    /// Fails closed: if the descriptor is not exactly where it is expected,
    /// we do not know what was downloaded.
    NotAnImage,
    /// The downloaded image's own version does not match what the manifest
    /// promised. This is the guard that stops an unbounded reflash loop when
    /// the two disagree.
    VersionMismatch,
}
