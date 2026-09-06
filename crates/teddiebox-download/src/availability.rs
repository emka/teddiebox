//! Whether a figure's story can be played this instant.
//!
//! The reducer asks a narrower question than the download path does. It only
//! needs to know whether to start playing or to ask for a fetch; *how* to
//! fetch — from zero or from where an interruption left off — stays in
//! `decide`, called by the download path with the same inputs. Two places
//! answering that would eventually disagree, and the disagreement would look
//! like a story restarting from the beginning.

use crate::cache::{decide, Cached, Decision};

/// `on_stock_card` is whether the file exists under `CONTENT/`, which is
/// looked up first and has no sidecar to vouch for it: shipped content is
/// complete by definition, and consulting it before the cache is what makes
/// writing to a stock card safe.
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
        let nothing_cached = Cached {
            sidecar: None,
            length_on_card: None,
        };
        assert!(playable_now(true, &nothing_cached));
    }

    #[test]
    fn a_complete_cached_file_plays() {
        let cached = Cached {
            sidecar: Some(sidecar(4096)),
            length_on_card: Some(4096),
        };
        assert!(playable_now(false, &cached));
    }

    /// A partial download is not playable, and deliberately reads the same as
    /// an absent one: the reducer asks for content either way, and the
    /// download path decides on its own whether that means resuming.
    #[test]
    fn a_partial_download_is_not_playable() {
        let cached = Cached {
            sidecar: Some(sidecar(4096)),
            length_on_card: Some(1024),
        };
        assert!(!playable_now(false, &cached));
    }

    #[test]
    fn a_file_with_no_sidecar_is_not_playable() {
        let cached = Cached {
            sidecar: None,
            length_on_card: Some(4096),
        };
        assert!(!playable_now(false, &cached));
    }

    #[test]
    fn nothing_anywhere_is_not_playable() {
        let cached = Cached {
            sidecar: None,
            length_on_card: None,
        };
        assert!(!playable_now(false, &cached));
    }

    #[test]
    fn a_file_longer_than_promised_is_not_playable() {
        let cached = Cached {
            sidecar: Some(sidecar(4096)),
            length_on_card: Some(8192),
        };
        assert!(!playable_now(false, &cached));
    }
}
