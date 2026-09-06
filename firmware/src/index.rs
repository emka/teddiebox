//! What the reducer is allowed to know about the card.
//!
//! `ContentIndex` is synchronous and infallible by design, and the only task
//! that can answer it without a cross-task round trip is the one that owns the
//! card. So this lives in the media task and does nothing but look.

use teddiebox_core::{ContentIndex, Position, TagUid};
use teddiebox_download::{content_path, playable_now, Cached, Sidecar, MAX_SIDECAR};

use crate::storage;

// Task 8 wires this into the media task's reducer loop; until then nothing
// constructs it and `-D warnings` would otherwise fail the lint gate.
#[allow(dead_code)]
pub struct CardIndex<'a> {
    card: &'a storage::Mounted,
}

// Same reason as above: nothing calls these until Task 8 wires the media
// task's reducer loop up to a real card.
#[allow(dead_code)]
impl<'a> CardIndex<'a> {
    pub fn new(card: &'a storage::Mounted) -> Self {
        Self { card }
    }

    /// Shipped content is complete by definition and carries no sidecar, so
    /// opening it is the whole test. The handle is closed immediately: this
    /// asks a question, it does not begin playback.
    fn on_stock_card(&self, directory: u32, file: u32) -> bool {
        match self.card.open_content(directory, file) {
            Ok((handle, _size)) => {
                self.card.close_file(handle);
                true
            }
            Err(_) => false,
        }
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

    /// Position memory is not built. The trait method is the seam it will
    /// fill; until then every placement starts the story from the beginning.
    fn saved_position(&self, _tag: TagUid) -> Position {
        Position::default()
    }
}
