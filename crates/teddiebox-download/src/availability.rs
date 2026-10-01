//! Whether a figure's story can be played this instant.
//!
//! The reducer only needs to know whether to play or to request a download.
//! *How* to download (from zero, or resuming) is decided by `decide`, which
//! this also uses, so the two cannot disagree.

use crate::cache::{decide, Cached, Decision};

/// `on_stock_card` is whether the file exists under `CONTENT/`. Such files
/// have no sidecar and are always treated as complete. They are checked
/// before the cache.
pub fn playable_now(on_stock_card: bool, cached: &Cached) -> bool {
    if on_stock_card {
        return true;
    }
    matches!(decide(cached), Decision::Play)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Sidecar;

    fn sidecar(length: u32) -> Sidecar {
        Sidecar { length, etag: None }
    }

    #[test]
    fn shipped_content_plays_without_a_sidecar() {
        // Given
        let nothing_cached = Cached {
            sidecar: None,
            length_on_card: None,
        };

        // When
        let playable = playable_now(true, &nothing_cached);

        // Then
        assert!(playable);
    }

    #[test]
    fn a_complete_cached_file_plays() {
        // Given
        let cached = Cached {
            sidecar: Some(sidecar(4096)),
            length_on_card: Some(4096),
        };

        // When
        let playable = playable_now(false, &cached);

        // Then
        assert!(playable);
    }

    /// A partial download is not playable, the same as a missing one. The
    /// download code decides whether to resume.
    #[test]
    fn a_partial_download_is_not_playable() {
        // Given
        let cached = Cached {
            sidecar: Some(sidecar(4096)),
            length_on_card: Some(1024),
        };

        // When
        let playable = playable_now(false, &cached);

        // Then
        assert!(!playable);
    }

    #[test]
    fn a_file_with_no_sidecar_is_not_playable() {
        // Given
        let cached = Cached {
            sidecar: None,
            length_on_card: Some(4096),
        };

        // When
        let playable = playable_now(false, &cached);

        // Then
        assert!(!playable);
    }

    #[test]
    fn nothing_anywhere_is_not_playable() {
        // Given
        let cached = Cached {
            sidecar: None,
            length_on_card: None,
        };

        // When
        let playable = playable_now(false, &cached);

        // Then
        assert!(!playable);
    }

    #[test]
    fn a_file_longer_than_promised_is_not_playable() {
        // Given
        let cached = Cached {
            sidecar: Some(sidecar(4096)),
            length_on_card: Some(8192),
        };

        // When
        let playable = playable_now(false, &cached);

        // Then
        assert!(!playable);
    }
}
