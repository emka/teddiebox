#![no_std]

mod header;
mod page;
mod reader;
mod source;
mod varint;

pub use header::TonieHeader;
pub use page::OggPage;
pub use reader::{TafReader, MAX_PACKET};
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
    /// Distinct from `NotAnOggPage`: the file is fine and the caller should
    /// retry with a bigger buffer, whereas a malformed page means stop.
    BufferTooSmall,
    /// The file is empty or not a whole number of pages. Distinct from
    /// `MalformedHeader`: this is a whole-file length problem detected before
    /// any header byte is read, and conflating the two misdirects debugging
    /// when a torn write leaves a partial trailing page.
    TruncatedFile,
}
