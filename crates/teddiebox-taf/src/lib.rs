#![no_std]

mod header;
mod page;
mod reader;
mod source;
mod varint;

pub use header::TonieHeader;
pub use reader::{TafReader, MAX_PACKET};

// `OggPage` (and its `Packets` iterator) is used only internally, by
// `reader::TafReader`. It stays `pub(crate)`: see the doc comment on
// `page::OggPage` for why this is a public-API safety boundary, not just
// tidiness.
pub(crate) use page::OggPage;
pub use source::{PageSource, SlicePages};

/// Every structure in a Tonie audio file is aligned to this boundary: the
/// header occupies page 0, and each Ogg page occupies exactly one page
/// thereafter. This is what makes page-indexed I/O possible on device.
pub const PAGE_SIZE: usize = 4096;

/// Upper bound on chapters in one file. Fixed because there is no allocator.
pub const MAX_CHAPTERS: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TafError {
    MalformedHeader,
    TooManyChapters,
    NotAnOggPage,
    PageOutOfRange,
    /// A well-formed packet is larger than the buffer the caller supplied.
    /// Distinct from `NotAnOggPage`: the file is fine, and the packet is
    /// left unconsumed, so a caller that can supply a *bigger* buffer on
    /// the next call will get this same packet rather than losing it.
    /// That promise is about the file and this reader, not about every
    /// caller: one stuck with a fixed-size buffer has no bigger buffer to
    /// retry with and must treat this as terminal. See `MAX_PACKET`'s doc
    /// for why that fixed size was chosen and how much headroom it has.
    BufferTooSmall,
    /// The file is empty or not a whole number of pages. Distinct from
    /// `MalformedHeader`: this is a whole-file length problem detected before
    /// any header byte is read, and conflating the two misdirects debugging
    /// when a torn write leaves a partial trailing page.
    TruncatedFile,
}

impl core::fmt::Display for TafError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let s = match self {
            TafError::MalformedHeader => "malformed TAF header",
            TafError::TooManyChapters => "too many chapters",
            TafError::NotAnOggPage => "not an Ogg page",
            TafError::PageOutOfRange => "page out of range",
            TafError::BufferTooSmall => "buffer too small for packet",
            TafError::TruncatedFile => "file is truncated or not a whole number of pages",
        };
        f.write_str(s)
    }
}

impl core::error::Error for TafError {}
