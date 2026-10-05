//! Whether a figure's story can be played this instant.
//!
//! The reducer only needs to know whether to play or to request a download.
//! *How* to download (from zero, or resuming) is decided by `decide`, which
//! this also uses, so the two cannot disagree.

use crate::cache::{decide, Cached, Decision};

/// Whether the story at one path shipped with the card, as opposed to being
/// downloaded.
///
/// Both kinds keep their audio under `CONTENT/<directory>/<file>`. A download
/// also has a sidecar under `CACHE/`, which records how long the audio is
/// meant to be. Stock audio has none and is always complete.
///
/// An empty file is never stock: a download creates its audio file before it
/// writes its sidecar, and a power loss in between must not leave an empty
/// story that counts as complete.
///
/// `audio_length` is the length of the file under `CONTENT/`, if there is
/// one; `has_sidecar` is whether a `.MET` exists, readable or not.
pub fn is_stock(audio_length: Option<u32>, has_sidecar: bool) -> bool {
    !has_sidecar && audio_length.is_some_and(|length| length > 0)
}

/// `on_stock_card` is the result of [`is_stock`]. Stock stories are always
/// complete, so they are checked before the sidecar.
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
    fn a_non_empty_file_without_a_sidecar_is_stock_content() {
        // Given
        let audio_length = Some(4096);
        let has_sidecar = false;

        // When
        let stock = is_stock(audio_length, has_sidecar);

        // Then
        assert!(stock);
    }

    /// A download keeps its sidecar in `CACHE/` while its audio sits under
    /// `CONTENT/`, so the sidecar is what tells it from shipped content.
    #[test]
    fn a_file_with_a_sidecar_is_a_download() {
        // Given
        let audio_length = Some(4096);
        let has_sidecar = true;

        // When
        let stock = is_stock(audio_length, has_sidecar);

        // Then
        assert!(!stock);
    }

    /// A download creates its audio file before it writes its sidecar. A power
    /// loss between the two leaves an empty file that must not read as a
    /// complete story.
    #[test]
    fn an_empty_file_without_a_sidecar_is_an_interrupted_download() {
        // Given
        let audio_length = Some(0);
        let has_sidecar = false;

        // When
        let stock = is_stock(audio_length, has_sidecar);

        // Then
        assert!(!stock);
    }

    #[test]
    fn no_file_is_not_stock_content() {
        // Given
        let audio_length = None;
        let has_sidecar = false;

        // When
        let stock = is_stock(audio_length, has_sidecar);

        // Then
        assert!(!stock);
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
