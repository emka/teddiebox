//! Where one figure's story got to, kept in RAM until it has to be written to
//! the card.
//!
//! Children lift a figure and put it back again and again. Writing the card on
//! every lift would cost a write each time, so the position is kept in RAM and
//! written to the card only when it would otherwise be lost: when another
//! figure needs the slot, or when the box shuts down.
//!
//! One slot, not a table, matching the stock box's "most recent figure"
//! behaviour. When a second figure replaces the first, the first figure's
//! position is written to the card, so nothing is lost.
//!
//! Trade-off: an unplanned stop (flat battery, crash, console reboot) loses
//! the position held in RAM.

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

    /// Stores a figure's position and returns the one it replaced, if any.
    ///
    /// Only a *different* figure replaces anything; the same figure just
    /// updates its page. The caller must write the returned position to the
    /// card, or it is lost.
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
    /// Check this before the card: the slot is always newer than the card.
    pub fn held(&self, tag: TagUid) -> Option<u32> {
        match self.slot {
            Some((held, page)) if held == tag => Some(page),
            _ => None,
        }
    }

    /// Forgets the position held for one story, identified by its ruid.
    ///
    /// Keyed by story, not by figure, because the caller (the media task)
    /// knows which file it is playing but not which figure asked for it. A
    /// console `play` of another story that ends must not clear a figure's
    /// position.
    pub fn forget(&mut self, ruid: u64) {
        if matches!(self.slot, Some((held, _)) if held.ruid() == ruid) {
            self.slot = None;
        }
    }

    /// Takes whatever is held, leaving the slot empty.
    ///
    /// Called just before the slot would be lost. A second call returns
    /// nothing, so nothing is written twice.
    pub fn take(&mut self) -> Option<Place> {
        self.slot.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real figure, `CONTENT/1D2E3F50/500304E0`.
    const LEO: TagUid = TagUid([0xE0, 0x04, 0x03, 0x50, 0x50, 0x3F, 0x2E, 0x1D]);
    /// Another real figure, *Abends im Walde*, `1E2F4051500304E0`.
    const WALDE: TagUid = TagUid([0xE0, 0x04, 0x03, 0x50, 0x51, 0x40, 0x2F, 0x1E]);

    #[test]
    fn a_fresh_slot_holds_nothing() {
        // Given
        let places = PendingPlace::new();

        // When
        let held = places.held(LEO);

        // Then
        assert_eq!(held, None);
    }

    #[test]
    fn a_remembered_place_is_held_for_that_figure() {
        // Given
        let mut places = PendingPlace::new();

        // When
        places.remember(LEO, 233);

        // Then
        assert_eq!(places.held(LEO), Some(233));
    }

    #[test]
    fn a_place_is_not_held_for_a_different_figure() {
        // Given
        let mut places = PendingPlace::new();

        // When
        places.remember(LEO, 233);

        // Then
        assert_eq!(places.held(WALDE), None);
    }

    /// Lifting a figure and putting it back must not cause a card write.
    #[test]
    fn the_same_figure_again_moves_the_page_on_and_displaces_nothing() {
        // Given
        let mut places = PendingPlace::new();
        places.remember(LEO, 233);

        // When
        let displaced = places.remember(LEO, 247);

        // Then
        assert_eq!(displaced, None);
        assert_eq!(places.held(LEO), Some(247));
    }

    /// The caller writes the replaced position to the card.
    #[test]
    fn a_different_figure_displaces_the_one_held_and_hands_it_back() {
        // Given
        let mut places = PendingPlace::new();
        places.remember(LEO, 284);

        // When
        let displaced = places.remember(WALDE, 12);

        // Then
        assert_eq!(displaced, Some((LEO, 284)));
    }

    #[test]
    fn a_displaced_figure_is_no_longer_held() {
        // Given
        let mut places = PendingPlace::new();
        places.remember(LEO, 284);

        // When
        places.remember(WALDE, 12);

        // Then
        assert_eq!(places.held(LEO), None);
        assert_eq!(places.held(WALDE), Some(12));
    }

    /// A finished story has no position to keep. The slot must be cleared
    /// too, because it is checked before the card.
    #[test]
    fn a_story_that_ended_is_forgotten() {
        // Given
        let mut places = PendingPlace::new();
        places.remember(LEO, 284);

        // When
        places.forget(LEO.ruid());

        // Then
        assert_eq!(places.held(LEO), None);
    }

    /// Keyed by story, not by figure: a console `play` that ends must not
    /// clear a figure's position.
    #[test]
    fn a_different_story_ending_leaves_the_slot_alone() {
        // Given
        let mut places = PendingPlace::new();
        places.remember(LEO, 284);

        // When
        places.forget(WALDE.ruid());

        // Then
        assert_eq!(places.held(LEO), Some(284));
    }

    #[test]
    fn taking_the_place_hands_it_back_and_empties_the_slot() {
        // Given
        let mut places = PendingPlace::new();
        places.remember(LEO, 284);

        // When
        let taken = places.take();

        // Then
        assert_eq!(taken, Some((LEO, 284)));
        assert_eq!(places.held(LEO), None);
    }

    /// A second call at shutdown must not write the same position again.
    #[test]
    fn taking_an_empty_slot_hands_back_nothing() {
        // Given
        let mut places = PendingPlace::new();

        // When
        let taken = places.take();

        // Then
        assert_eq!(taken, None);
    }
}
