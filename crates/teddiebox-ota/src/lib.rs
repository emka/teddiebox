#![no_std]

//! What an over-the-air update decides, separated from how its bytes move.
//!
//! Nothing here does I/O. The firmware supplies the socket, the flash region
//! and the reboot; this crate owns the questions that have right and wrong
//! answers — whether a manifest is well formed, whether its version differs
//! from ours, whether the digest matched, which sector a write lands in — so
//! that all of them can be tested without a box.

mod boot;
mod decide;
mod digest;
mod image;
mod manifest;
mod sink;
mod stage;
mod url;
mod verify;

pub use boot::{boot_action, BootAction, SlotState};
pub use decide::{decide, Decision, Refusal};
pub use image::{image_version, may_activate};
pub use manifest::{Manifest, FILENAME, MAX_IMAGE_PATH, MAX_MANIFEST, MAX_VERSION};
pub use sink::{FlashRegionLike, Sectors, SinkError, SECTOR};
pub use stage::ImageWriter;
pub use url::{resolve_image, split, UpdateUrl, MAX_HOST, MAX_PATH};
pub use verify::verify;

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
    /// A `length` value is not a `u32`: not numeric, negative, or too large.
    MalformedLength,
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
    /// promised. Refusing it stops the box from reflashing the same image
    /// forever.
    VersionMismatch,
    /// The body that arrived is not as long as the manifest's `length`.
    LengthMismatch,
    /// The body's SHA-256 is not the manifest's `sha256`: the download was
    /// corrupted, or the image was replaced after the manifest was written.
    DigestMismatch,
    /// An `update_url` did not start with `https://`. teddyCloud only speaks
    /// TLS, and a plain-HTTP request to it hangs instead of failing, so the
    /// box would look frozen rather than misconfigured.
    NotHttps,
    /// An `update_url`, or an `image` resolved against a manifest path, is
    /// not shaped the way this crate requires: no `/` after the host, a path
    /// naming a directory rather than a file, an empty host, a query or
    /// fragment riding along, a byte outside the conservative path set, or a
    /// `..` segment.
    ///
    /// The `..` refusal is **hygiene, not a security boundary** — an absolute
    /// `image` is used as-is by design, so nothing has to go through `..` to
    /// name a path outside the manifest's directory. See the "Trust model"
    /// section on the [`url`] module for what actually holds.
    MalformedUrl,
}
