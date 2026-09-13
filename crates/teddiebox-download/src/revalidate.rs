//! Whether a story already on the card is still the story the server has.
//!
//! There is no ETag to match — teddyCloud sends none, on any route — so the
//! question is answered with the one number it does state: how long the file
//! is. Weaker than an ETag and honest about it: a figure's audio does not
//! change silently, and a re-encode to exactly the same length reads as
//! current.
//!
//! The rule that matters more than the comparison is what happens when there
//! is no answer. A server that cannot be reached, a figure the cloud has never
//! heard of, a reply with no length in it: none of those is evidence against
//! a file that is sitting on the card and plays. Revalidation may never make a
//! working box worse than it was offline.

use crate::Sidecar;
use teddiebox_cloud::Probed;

/// How many figures are remembered as asked-about since boot.
///
/// A session is one child and the figures within reach of them. Eight is
/// generous for that, and the cost of overflowing is one extra probe on the
/// ninth figure — a few hundred bytes and a radio that was about to be raised
/// anyway.
pub const REMEMBERED: usize = 8;

/// Whether the cached file is out of date and must be fetched again.
///
/// Only a length the server actually *stated*, and which differs from what the
/// sidecar promised, makes a file stale. Everything else is `false`, which
/// means the cached copy plays.
pub fn is_stale(cached: &Sidecar, probed: Probed) -> bool {
    match probed {
        Probed::Length(total) => total != cached.length,
        // The server has no story for this figure at all. That is an answer
        // about the *server*, and a reason to keep what is on the card rather
        // than to throw it away — the card copy may be the last one in
        // existence.
        Probed::NoContent => false,
        // It answered, and said nothing about length. Nothing was contradicted.
        Probed::Unstated => false,
    }
}

/// Which figures have already been asked about since the box booted.
///
/// Asking costs the radio, which is the largest consumer on this pack and adds
/// seconds before a story starts. The box switches itself off after five idle
/// minutes, so one boot is roughly one session, and once per session per
/// figure is the cadence. It also needs no clock, which this box does not
/// have.
///
/// A figure is remembered when its answer *arrives*, whatever the answer was.
/// A server that was unreachable a moment ago is unreachable for the rest of a
/// five-minute session, and asking it again on the next placement spends the
/// radio to be told the same thing.
#[derive(Debug, Default)]
pub struct Asked {
    /// Ruids, oldest first. The oldest is evicted when a ninth arrives.
    ruids: heapless::Deque<u64, REMEMBERED>,
}

impl Asked {
    pub const fn new() -> Self {
        Self {
            ruids: heapless::Deque::new(),
        }
    }

    pub fn contains(&self, ruid: u64) -> bool {
        self.ruids.iter().any(|&seen| seen == ruid)
    }

    /// Records that this figure has been asked about. Asking twice is not an
    /// error and does not push anything else out.
    pub fn remember(&mut self, ruid: u64) {
        if self.contains(ruid) {
            return;
        }
        if self.ruids.is_full() {
            let _ = self.ruids.pop_front();
        }
        let _ = self.ruids.push_back(ruid);
    }

    /// Forgets everything, so the next placement asks again.
    ///
    /// Exists for the console: a bench that has just changed a file on the
    /// server needs to see the box notice, without power-cycling it first.
    pub fn forget_all(&mut self) {
        self.ruids.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use teddiebox_cloud::ETag;

    fn sidecar(length: u32) -> Sidecar {
        Sidecar { length, etag: None }
    }

    /// The real numbers from 2026-09-13: `1d2e3f50500304e0` is 37,912,939
    /// bytes on the server and the same on the card.
    #[test]
    fn a_length_that_matches_is_current() {
        assert!(!is_stale(&sidecar(37_912_939), Probed::Length(37_912_939)));
    }

    #[test]
    fn a_length_that_differs_is_stale_whichever_way_it_moved() {
        assert!(is_stale(&sidecar(37_912_939), Probed::Length(37_912_940)));
        assert!(is_stale(&sidecar(37_912_939), Probed::Length(1_024)));
    }

    /// The case that decides whether this feature is safe to ship. A box
    /// carried out of range of its server must keep playing what it has.
    #[test]
    fn a_server_with_no_story_never_discards_the_one_on_the_card() {
        assert!(!is_stale(&sidecar(37_912_939), Probed::NoContent));
    }

    #[test]
    fn an_answer_without_a_length_changes_nothing() {
        assert!(!is_stale(&sidecar(37_912_939), Probed::Unstated));
    }

    /// An etag on the sidecar is not consulted: this server sends none, and a
    /// stale etag left over from an earlier design must not outvote a length
    /// that was measured this minute.
    #[test]
    fn the_recorded_etag_does_not_enter_into_it() {
        let with_etag = Sidecar {
            length: 4_096,
            etag: Some(ETag::try_from("\"v1\"").unwrap()),
        };
        assert!(is_stale(&with_etag, Probed::Length(8_192)));
        assert!(!is_stale(&with_etag, Probed::Length(4_096)));
    }

    #[test]
    fn a_figure_asked_about_once_is_not_asked_again() {
        let mut asked = Asked::new();
        assert!(!asked.contains(0x1D2E_3F50_5003_04E0));
        asked.remember(0x1D2E_3F50_5003_04E0);
        assert!(asked.contains(0x1D2E_3F50_5003_04E0));
    }

    #[test]
    fn remembering_the_same_figure_twice_costs_no_room() {
        let mut asked = Asked::new();
        for _ in 0..REMEMBERED * 2 {
            asked.remember(1);
        }
        asked.remember(2);
        assert!(asked.contains(1), "one crowding itself out is the bug");
        assert!(asked.contains(2));
    }

    /// The ninth figure of a session costs the first one its place, and the
    /// cost of that is one extra probe. Losing the *newest* instead would
    /// re-ask the figure currently in a child's hand, every time.
    #[test]
    fn a_ninth_figure_pushes_out_the_oldest() {
        let mut asked = Asked::new();
        for ruid in 0..REMEMBERED as u64 {
            asked.remember(ruid);
        }
        asked.remember(99);
        assert!(!asked.contains(0));
        assert!(asked.contains(1));
        assert!(asked.contains(99));
    }

    /// A bench that has just replaced a file on the server needs the box to
    /// notice without a power cycle.
    #[test]
    fn forgetting_makes_every_figure_ask_again() {
        let mut asked = Asked::new();
        asked.remember(7);
        asked.forget_all();
        assert!(!asked.contains(7));
    }
}
