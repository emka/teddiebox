#![no_std]

mod header;
mod page;
mod reader;
mod source;
mod varint;

pub use header::TonieHeader;
pub use reader::{TafReader, MAX_PACKET};

// Only `reader::TafReader` should walk packets: `PacketCursor::next` reports a
// corrupt page as an error, and a public version would invite callers to
// ignore it.
pub(crate) use page::PacketCursor;
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
    /// The file is fine and the packet is not consumed, so retrying with a
    /// bigger buffer returns the same packet. A caller with a fixed-size
    /// buffer must treat this as fatal. See `MAX_PACKET` for how that size
    /// was chosen.
    BufferTooSmall,
    /// The file is shorter than the header page, or shorter than the stream
    /// the header declares. This is a length problem, not a content problem
    /// like `MalformedHeader`; it is what an interrupted write looks like.
    ///
    /// Detection works in whole pages: [`PageSource`] reports how many pages
    /// exist, not how many bytes. A file cut short inside its *last* page
    /// still looks complete. The last page of a real file is often short
    /// too, so a byte length would not tell the two apart anyway.
    TruncatedFile,
    /// A structurally valid Ogg page that belongs to a different stream.
    ///
    /// Unlike `NotAnOggPage`, the bytes are a valid page — just not one from
    /// this file's stream. An interrupted write can leave a block of an older,
    /// longer recording behind; without this check it would decode fine and
    /// the child would hear the end of the old story.
    WrongStream,
    /// The [`PageSource`] could not read a page that is within range.
    ///
    /// The storage failed, not the file: on the device this is an SD read
    /// error, a CRC failure, or a card timeout. The source's own error is not
    /// carried here, to keep this type `Copy` and independent of the source,
    /// so a source with more detail should log it before returning.
    Io,
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
            TafError::WrongStream => "Ogg page belongs to a different stream",
            TafError::Io => "could not read a page from the source",
        };
        f.write_str(s)
    }
}

impl core::error::Error for TafError {}
