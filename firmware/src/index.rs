//! What the reducer is allowed to know about the card.
//!
//! `ContentIndex` is synchronous and cannot fail, and only the task that owns
//! the card can answer it directly. So this lives in the media task and only
//! reads.

use teddiebox_core::position::{self, MAX_POSITION};
use teddiebox_core::{ContentIndex, Position, TagUid};
use teddiebox_download::{content_path, is_stock, playable_now, Cached, Sidecar, MAX_SIDECAR};

use crate::storage;

pub struct CardIndex<'a> {
    card: &'a storage::Mounted,
}

impl<'a> CardIndex<'a> {
    pub fn new(card: &'a storage::Mounted) -> Self {
        Self { card }
    }

    /// Whether the card ships this story, as opposed to it being downloaded.
    ///
    /// Both kinds are under `CONTENT/`. Stock content is always complete and
    /// has no sidecar; see [`is_stock`].
    ///
    /// Public because whoever handles `Action::Play` must know which kind it
    /// plays, since a download's position is saved apart from stock content.
    pub fn on_stock_card(&self, directory: u32, file: u32) -> bool {
        is_stock(
            self.card.audio_length(directory, file),
            self.card.has_sidecar(directory, file),
        )
    }

    /// Records where this story should resume.
    ///
    /// Here rather than in `perform`, because this type already knows a tag's
    /// path and whether it is stock or cached.
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
    /// `None` for stock content, a story not on the card, or an unreadable
    /// sidecar: in each case there is no length to compare with the server.
    pub fn sidecar(&self, tag: TagUid) -> Option<Sidecar> {
        let path = content_path(tag.0);
        self.cached(path.directory, path.file).sidecar
    }

    fn cached(&self, directory: u32, file: u32) -> Cached {
        let mut buffer = [0u8; MAX_SIDECAR];
        // A sidecar that is not valid UTF-8 cannot be trusted, and is
        // treated as no sidecar.
        let sidecar = self
            .card
            .read_sidecar(directory, file, &mut buffer)
            .and_then(|len| core::str::from_utf8(&buffer[..len]).ok())
            .and_then(|text| Sidecar::parse(text).ok());

        Cached {
            sidecar,
            length_on_card: self.card.audio_length(directory, file),
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

        // A card read error counts as "not here", so the box will try the
        // network even if the story is on the card. The trait cannot return
        // errors, so this is logged instead.
        if !available && cached.length_on_card.is_some() && cached.sidecar.is_none() {
            esp_println::println!(
                "teddiebox: plate /CONTENT/{:08X}/{:08X} has no readable sidecar — refetching",
                path.directory,
                path.file
            );
        }

        available
    }

    /// Whether this figure's story should be checked against the server
    /// before it plays.
    ///
    /// No check is needed for:
    /// - **Stock content**: a file under `CONTENT/` has no sidecar and was
    ///   never downloaded.
    /// - **An incomplete download**: it goes to the network anyway.
    /// - **A figure already checked since boot**: Wi-Fi uses the most battery,
    ///   and one boot is roughly one play session, since the box switches off
    ///   after five idle minutes.
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

    /// Where this story should resume: the position held in RAM if there is
    /// one, otherwise the one saved on the card.
    ///
    /// An unreadable, missing or malformed file gives `Start`; the card
    /// cannot be trusted.
    fn saved_position(&self, tag: TagUid) -> Position {
        // RAM first: it is always newer than the card, which is only written
        // when the RAM copy is about to be lost.
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
