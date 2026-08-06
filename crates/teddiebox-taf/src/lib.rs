#![no_std]

mod header;
mod page;
mod reader;
mod source;
mod varint;

pub use header::TonieHeader;
pub use reader::{TafReader, MAX_PACKET};

// The lacing walk is internal to `reader::TafReader`. It stays `pub(crate)`
// because its `next` reports a corrupt page as an error the caller must
// handle, and a public version would invite callers who ignore it.
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
    /// Distinct from `NotAnOggPage`: the file is fine, and the packet is
    /// left unconsumed, so a caller that can supply a *bigger* buffer on
    /// the next call will get this same packet rather than losing it.
    /// That promise is about the file and this reader, not about every
    /// caller: one stuck with a fixed-size buffer has no bigger buffer to
    /// retry with and must treat this as terminal. See `MAX_PACKET`'s doc
    /// for why that fixed size was chosen and how much headroom it has.
    BufferTooSmall,
    /// The file is shorter than the header page, or shorter than the stream
    /// the header declares. Distinct from `MalformedHeader`: this is a
    /// length problem rather than a content one, and conflating the two
    /// misdirects debugging when a torn write cuts a file short.
    ///
    /// Detection is page-granular, which is the limit of a page-indexed
    /// source: [`PageSource`] reports how many pages exist, not how many
    /// bytes, so a file cut partway through its *final* page still looks
    /// complete. Closing that would mean giving the trait a byte length,
    /// which is a file-system notion this abstraction deliberately does not
    /// have. A real file's final page is legitimately short, so the length
    /// alone cannot distinguish the two cases anyway.
    TruncatedFile,
    /// A structurally valid Ogg page that belongs to a different stream.
    ///
    /// Distinct from `NotAnOggPage`, which says the bytes are not a page at
    /// all. These bytes are a perfectly good page — it is simply not part of
    /// this file's stream, which is what a torn write leaving a block from a
    /// previous, longer recording looks like. Nothing about its shape gives
    /// it away, so without this check it decodes cleanly and the child hears
    /// the end of the previous story.
    WrongStream,
    /// The [`PageSource`] could not read a page that is within range.
    ///
    /// Says nothing about the file, only that the medium would not produce
    /// it: on device this is an SD I/O fault, a CRC failure, or a card
    /// timeout. Distinct from `PageOutOfRange` and `MalformedHeader`
    /// because those accuse the file of being wrong, and following that
    /// accusation is wasted effort when the file is fine and the card is
    /// not. The source's own error is deliberately not carried here — this
    /// type is `Copy` and source-agnostic — so a driver with more to say
    /// should log it before returning.
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
