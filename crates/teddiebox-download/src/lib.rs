#![no_std]

//! What a download decides, separated from how its bytes move.
//!
//! Nothing here does I/O. The firmware supplies the card and the socket; this
//! crate owns the questions that have right and wrong answers — whether a file
//! on the card is whole, where a resumed download continues, how far ahead of
//! the decoder the writer has got — so that all of them can be tested without
//! a box.

mod cache;
mod reconcile;
mod sidecar;
mod units;
mod window;
mod writer;

pub use cache::{decide, Cached, Decision};
pub use reconcile::{reconcile, Action, Mismatch};
pub use sidecar::{Sidecar, MAX_SIDECAR};
pub use units::{Bytes, Pages};
pub use window::{may_decode, Window, WindowError};
pub use writer::{ContentSink, Writer};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadError {
    /// The sidecar is unreadable, which means the content beside it cannot be
    /// trusted to be complete. Treated as no sidecar at all.
    MalformedSidecar,
}
