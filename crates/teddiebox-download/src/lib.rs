#![no_std]

//! The decisions a download makes, separate from moving its bytes.
//!
//! Nothing here does I/O. The firmware provides the card and the socket. This
//! crate answers questions like whether a file on the card is complete, where
//! a resumed download continues, and how far ahead of the decoder the writer
//! is, so all of them can be tested on the host.

mod availability;
mod cache;
mod content_path;
mod handshake;
mod outcome;
mod reconcile;
mod revalidate;
mod sidecar;
mod throttle;
mod transfer;
mod units;
mod window;
mod writer;

pub use availability::playable_now;
pub use cache::{
    decide, is_whole, place, revalidate, Cached, Decision, Freshness, Landing, Placement,
};
pub use content_path::{content_path, hex8, ContentPath};
pub use handshake::{CardSays, Handshake, Step, DEADLINE_MS, RETRY_MS};
pub use outcome::{Outcome, Outcomes};
pub use reconcile::{reconcile, Action, Mismatch};
pub use revalidate::{is_stale, Answer, Asked, Revalidation, Settled, PATIENCE_MS, REMEMBERED};
pub use sidecar::{Sidecar, MAX_SIDECAR};
pub use throttle::{next_step, Continue, Throttle};
pub use transfer::{Finished, Head, Plan, Transfer, Work};
pub use units::{Bytes, Pages};
pub use window::{may_decode, Window, WindowError};
pub use writer::{ContentSink, Writer};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadError {
    /// The sidecar cannot be read, so the content file cannot be trusted to
    /// be complete. Treated as no sidecar.
    MalformedSidecar,
}
