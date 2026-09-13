//! What the reducer is allowed to know about the card.
//!
//! `ContentIndex` is synchronous and infallible by design, and the only task
//! that can answer it without a cross-task round trip is the one that owns the
//! card. So this lives in the media task and does nothing but look.

use teddiebox_core::position::{self, MAX_POSITION};
use teddiebox_core::{ContentIndex, Position, TagUid};
use teddiebox_download::{content_path, playable_now, Cached, Sidecar, MAX_SIDECAR};

use crate::storage;

pub struct CardIndex<'a> {
    card: &'a storage::Mounted,
}

impl<'a> CardIndex<'a> {
    pub fn new(card: &'a storage::Mounted) -> Self {
        Self { card }
    }

    /// Whether the card ships this story under `CONTENT/`.
    ///
    /// Shipped content is complete by definition and carries no sidecar, so
    /// opening it is the whole test. The handle is closed immediately: this
    /// asks a question, it does not begin playback.
    ///
    /// Public because the same answer decides more than availability: a story
    /// under `CONTENT/` and a downloaded one under `CACHE/` are played by two
    /// different requests, and whoever acts on `Action::Play` has to pick one.
    pub fn on_stock_card(&self, directory: u32, file: u32) -> bool {
        match self.card.open_content(directory, file) {
            Ok((handle, _size)) => {
                self.card.close_file(handle);
                true
            }
            Err(_) => false,
        }
    }

    /// Records where this story should resume.
    ///
    /// Story-scoped card knowledge belongs here rather than in `perform`: this
    /// type already knows how a tag becomes a path and whether that path is
    /// stock or cached, and it is the only thing holding the card.
    pub fn remember(&self, tag: TagUid, page: u32) -> Result<(), &'static str> {
        let path = content_path(tag.0);
        let stock = self.on_stock_card(path.directory, path.file);
        let mut out = [0u8; MAX_POSITION];
        let len = position::render(page, &mut out);
        self.card
            .write_position(stock, path.directory, path.file, &out[..len])
    }

    /// What the card's sidecar promises this story's whole length is.
    ///
    /// `None` for stock content, for a story that is not on the card, and for
    /// a sidecar that cannot be read — three different things, and all three
    /// mean the same here: there is no promise to compare the server against.
    pub fn sidecar(&self, tag: TagUid) -> Option<Sidecar> {
        let path = content_path(tag.0);
        self.cached(path.directory, path.file).sidecar
    }

    fn cached(&self, directory: u32, file: u32) -> Cached {
        let mut buffer = [0u8; MAX_SIDECAR];
        // `Sidecar::parse` takes `&str`, not bytes: a sidecar that is not
        // valid UTF-8 is a sidecar that cannot be trusted, and `decide`
        // already treats an unreadable one as no sidecar at all.
        let sidecar = self
            .card
            .read_sidecar(directory, file, &mut buffer)
            .and_then(|len| core::str::from_utf8(&buffer[..len]).ok())
            .and_then(|text| Sidecar::parse(text).ok());

        Cached {
            sidecar,
            length_on_card: self.card.cache_length(directory, file),
        }
    }
}

impl ContentIndex for CardIndex<'_> {
    fn is_available(&self, tag: TagUid) -> bool {
        let path = content_path(tag.0);

        if self.on_stock_card(path.directory, path.file) {
            return true;
        }

        let cached = self.cached(path.directory, path.file);
        let available = playable_now(false, &cached);

        // A card that cannot be read reads as "not here", which sends the box
        // to the network for a story that is sitting on the card. That is the
        // known cost of an infallible trait; it is at least loud in the log.
        if !available && cached.length_on_card.is_some() && cached.sidecar.is_none() {
            esp_println::println!(
                "teddiebox: plate /CACHE/{:08X}/{:08X} has no readable sidecar — refetching",
                path.directory,
                path.file
            );
        }

        available
    }

    /// Whether this figure's story should be checked against the server
    /// before it plays.
    ///
    /// Three conditions, and each one removes a case that would cost the radio
    /// for nothing. **Stock content never asks**: a file under `CONTENT/` has
    /// no sidecar to compare a length against and was never downloaded.
    /// **An incomplete download never asks**: it is already going to the
    /// network, and `decide` knows whether that is a fetch or a resume.
    /// **A figure asked about since boot never asks again**: the radio is the
    /// largest consumer on this pack, and one boot is roughly one session
    /// because the box switches itself off after five idle minutes.
    fn wants_revalidation(&self, tag: TagUid) -> bool {
        let path = content_path(tag.0);
        if self.on_stock_card(path.directory, path.file) {
            return false;
        }
        if crate::already_asked(tag) {
            return false;
        }
        playable_now(false, &self.cached(path.directory, path.file))
    }

    /// The card's tier: which chapter this story was in when the box last
    /// stopped. It survives a power cycle, and it is all that survives one.
    ///
    /// An unreadable, absent or malformed file reads as `Start`. The card is
    /// not a trusted input, and no byte on it should be able to strand a
    /// figure at a chapter its story does not have.
    fn saved_position(&self, tag: TagUid) -> Position {
        // RAM first: it is newer than the card by construction, because the
        // card is only written once this is about to be lost. A figure lifted
        // and put straight back — which is most of what happens to a figure —
        // never touches the card at all.
        if let Some(page) = crate::held_place(tag) {
            return Position::Exact { page };
        }

        let path = content_path(tag.0);
        let stock = self.on_stock_card(path.directory, path.file);
        let mut buffer = [0u8; MAX_POSITION];
        self.card
            .read_position(stock, path.directory, path.file, &mut buffer)
            .and_then(|len| core::str::from_utf8(&buffer[..len]).ok())
            .map(position::parse)
            .unwrap_or(Position::Start)
    }
}
