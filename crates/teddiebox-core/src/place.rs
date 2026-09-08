//! Where one figure's story got to, held until the card has to be told.
//!
//! Children lift a figure and put it straight back, over and over. Writing the
//! card on every lift makes that ordinary act cost a write, so this holds the
//! place in RAM and the card is written only where the place would otherwise
//! be lost: another figure needing the slot, or the box shutting down. Measured
//! at the bench on 2026-09-08 — six lifts, two writes, both displacements.
//!
//! One slot, not a table. "The most recent figure" is the stock behaviour being
//! matched, and a second figure evicting the first is correct rather than a
//! limitation: the evicted figure's place goes to the card on its way out, so
//! nothing is dropped, it only stops being free to read back.
//!
//! What it gives up, deliberately: an ending nobody chose — a flat battery, a
//! crash, a console reboot — loses whatever RAM is holding. A dead box is not
//! being listened to, so the cost is one restart rather than a child losing
//! their place mid-story.

use crate::TagUid;

/// A figure and the page its story reached.
pub type Place = (TagUid, u32);

/// The one place held in RAM, as a write-back cache of a single entry.
#[derive(Debug, Default)]
pub struct PendingPlace {
    slot: Option<Place>,
}

impl PendingPlace {
    pub const fn new() -> Self {
        Self { slot: None }
    }

    /// Holds a figure's place, handing back the one it displaced.
    ///
    /// Only a *different* figure displaces anything: the same figure moving on
    /// through its story just moves the page on, which is what keeps repeated
    /// lifts free. What comes back is owed to the card — its place is about to
    /// become unreachable and this is the last moment anything knows it.
    pub fn remember(&mut self, tag: TagUid, page: u32) -> Option<Place> {
        let displaced = match self.slot {
            Some((held, held_page)) if held != tag => Some((held, held_page)),
            _ => None,
        };
        self.slot = Some((tag, page));
        displaced
    }

    /// What is held for this figure, if anything.
    ///
    /// Asked before the card, because the slot is newer by construction: it is
    /// written the moment a figure comes off, and the card only learns later.
    pub fn held(&self, tag: TagUid) -> Option<u32> {
        match self.slot {
            Some((held, page)) if held == tag => Some(page),
            _ => None,
        }
    }

    /// Drops the place held for one story, named the way the card names it.
    ///
    /// Keyed by the story rather than by the figure because the caller is the
    /// media task, which knows the path it is playing from and not which figure
    /// asked for it — and because a console `play` of another story running to
    /// its end must not forget a figure's place.
    pub fn forget(&mut self, ruid: u64) {
        if matches!(self.slot, Some((held, _)) if held.ruid() == ruid) {
            self.slot = None;
        }
    }

    /// Takes whatever is held, leaving the slot empty.
    ///
    /// Called where the slot is about to be lost for good, so it is idempotent
    /// by construction: a second call has nothing to hand back and therefore
    /// writes nothing.
    pub fn take(&mut self) -> Option<Place> {
        self.slot.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bench figure, `CONTENT/1D2E3F50/500304E0`.
    const LEO: TagUid = TagUid([0xE0, 0x04, 0x03, 0x50, 0x50, 0x3F, 0x2E, 0x1D]);
    /// *Abends im Walde*, `1E2F4051500304E0` — the resume test's second file.
    const WALDE: TagUid = TagUid([0xE0, 0x04, 0x03, 0x50, 0x51, 0x40, 0x2F, 0x1E]);

    #[test]
    fn a_fresh_slot_holds_nothing() {
        assert_eq!(PendingPlace::new().held(LEO), None);
    }

    #[test]
    fn a_remembered_place_is_held_for_that_figure() {
        let mut places = PendingPlace::new();
        places.remember(LEO, 233);
        assert_eq!(places.held(LEO), Some(233));
    }

    #[test]
    fn a_place_is_not_held_for_a_different_figure() {
        let mut places = PendingPlace::new();
        places.remember(LEO, 233);
        assert_eq!(places.held(WALDE), None);
    }

    /// The whole reason this is a cache rather than a write-through. Lifting a
    /// figure and putting it back is most of what happens to a figure, and it
    /// must not reach the card.
    #[test]
    fn the_same_figure_again_moves_the_page_on_and_displaces_nothing() {
        let mut places = PendingPlace::new();
        places.remember(LEO, 233);
        assert_eq!(places.remember(LEO, 247), None);
        assert_eq!(places.held(LEO), Some(247));
    }

    /// The displaced entry is what the caller writes to the card: this is the
    /// last moment anything knows it.
    #[test]
    fn a_different_figure_displaces_the_one_held_and_hands_it_back() {
        let mut places = PendingPlace::new();
        places.remember(LEO, 284);
        assert_eq!(places.remember(WALDE, 12), Some((LEO, 284)));
    }

    #[test]
    fn a_displaced_figure_is_no_longer_held() {
        let mut places = PendingPlace::new();
        places.remember(LEO, 284);
        places.remember(WALDE, 12);
        assert_eq!(places.held(LEO), None);
        assert_eq!(places.held(WALDE), Some(12));
    }

    /// A story that reached its end has no place worth keeping, and the slot
    /// must go with the card: a stale entry outranks the file just zeroed,
    /// because [`PendingPlace`] is asked first.
    #[test]
    fn a_story_that_ended_is_forgotten() {
        let mut places = PendingPlace::new();
        places.remember(LEO, 284);
        places.forget(LEO.ruid());
        assert_eq!(places.held(LEO), None);
    }

    /// Keyed by the story, not by whoever is on the plate — the media task
    /// knows where it is playing from and not which figure asked for it. A
    /// console `play` running to its end must not forget a figure's place.
    #[test]
    fn a_different_story_ending_leaves_the_slot_alone() {
        let mut places = PendingPlace::new();
        places.remember(LEO, 284);
        places.forget(WALDE.ruid());
        assert_eq!(places.held(LEO), Some(284));
    }

    #[test]
    fn taking_the_place_hands_it_back_and_empties_the_slot() {
        let mut places = PendingPlace::new();
        places.remember(LEO, 284);
        assert_eq!(places.take(), Some((LEO, 284)));
        assert_eq!(places.held(LEO), None);
    }

    /// What makes the shutdown flush idempotent: it is called where the slot is
    /// about to be lost for good, and a second call must write nothing rather
    /// than write the same place again.
    #[test]
    fn taking_an_empty_slot_hands_back_nothing() {
        assert_eq!(PendingPlace::new().take(), None);
    }
}
