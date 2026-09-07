//! Whether a figure is on the plate, from readings that disagree.
//!
//! A tag drops out of inventory for reasons that have nothing to do with a
//! child's hand — the reader's own `T2_QUIET_US` work exists because a correct
//! transaction can go unanswered — so a single missed read must not stop a
//! story, and a single stray read must not start one.
//!
//! Nor stop one. A reading can be wrong as well as absent: on 2026-09-07 the
//! reader was measured returning a UID that had never been on the plate while
//! the radio was busy, and because a changed UID used to announce a departure
//! on sight, one such reading stopped the story and restarted it from the
//! beginning. Every transition here now costs several agreeing readings —
//! arrival, departure by absence, and departure by replacement alike.

use crate::TagUid;

/// Consecutive readings of the same tag before it counts as arrived.
///
/// Provisional. Nothing has calibrated this: it trades how fast a placement
/// feels against a false arrival. The precedent is `sounds::READINGS_TO_AGREE`,
/// which is 4 for a quantity that changes far more slowly than a hand.
pub const ARRIVALS_TO_AGREE: u8 = 2;

/// Consecutive empty readings before a tag counts as gone.
///
/// Provisional, and deliberately larger than `ARRIVALS_TO_AGREE`: stopping a
/// story that should still be playing is a worse failure than starting one
/// slightly late.
pub const MISSES_TO_LEAVE: u8 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TagEvent {
    Arrived(TagUid),
    Left,
}

/// What the filter believes right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Nothing has been on the plate, or the last thing left.
    Empty,
    /// Readings of this tag are accumulating but have not yet agreed.
    Arriving { tag: TagUid, seen: u8 },
    /// Announced as present. `missed` counts consecutive empty readings.
    Present { tag: TagUid, missed: u8 },
    /// A figure is present and something else has been read. Held here until
    /// the newcomer is seen often enough to be believed, because one reading
    /// of a UID that was never on the plate is noise — and under radio traffic
    /// the reader produces it.
    Swapping {
        from: TagUid,
        missed: u8,
        to: TagUid,
        seen: u8,
    },
}

#[derive(Debug)]
pub struct Presence {
    state: State,
    arrivals_to_agree: u8,
    misses_to_leave: u8,
}

impl Presence {
    pub const fn new(arrivals_to_agree: u8, misses_to_leave: u8) -> Self {
        Self {
            state: State::Empty,
            arrivals_to_agree,
            misses_to_leave,
        }
    }

    /// Feeds one poll's result and returns an event only when the reading has
    /// settled. A different tag while one is present reports the departure
    /// first; its arrival follows on later calls, so a swap is never a single
    /// unbroken presence.
    pub fn feed(&mut self, seen: Option<TagUid>) -> Option<TagEvent> {
        match (self.state, seen) {
            (State::Empty, None) => None,

            (State::Empty, Some(tag)) => {
                self.state = State::Arriving { tag, seen: 1 };
                self.settle_arrival(tag, 1)
            }

            (State::Arriving { .. }, None) => {
                self.state = State::Empty;
                None
            }

            (State::Arriving { tag, seen }, Some(now)) if now == tag => {
                let seen = seen.saturating_add(1);
                self.state = State::Arriving { tag, seen };
                self.settle_arrival(tag, seen)
            }

            // A different tag restarts the count rather than inheriting it.
            // No departure is announced because nothing had arrived yet, so this
            // reading counts toward the newcomer's tally.
            (State::Arriving { .. }, Some(now)) => {
                self.state = State::Arriving { tag: now, seen: 1 };
                self.settle_arrival(now, 1)
            }

            (State::Present { tag, missed }, None) => {
                let missed = missed.saturating_add(1);
                if missed >= self.misses_to_leave {
                    self.state = State::Empty;
                    Some(TagEvent::Left)
                } else {
                    self.state = State::Present { tag, missed };
                    None
                }
            }

            (State::Present { tag, .. }, Some(now)) if now == tag => {
                self.state = State::Present { tag, missed: 0 };
                None
            }

            // Another figure while one is present. The departure is not
            // announced yet: a swap has to agree the same way an arrival does,
            // or a single corrupt reading ends a story that never stopped.
            (State::Present { tag, missed }, Some(now)) => {
                self.state = State::Swapping {
                    from: tag,
                    missed,
                    to: now,
                    seen: 1,
                };
                self.settle_swap(now, 1)
            }

            // The newcomer again. Once enough readings agree, the figure that
            // was there has genuinely gone.
            (State::Swapping { to, seen, .. }, Some(now)) if now == to => {
                let seen = seen.saturating_add(1);
                self.settle_swap(to, seen)
            }

            // The original answered after all, so the odd reading was noise.
            // The miss count goes with it: this was a reading, not a silence.
            (State::Swapping { from, .. }, Some(now)) if now == from => {
                self.state = State::Present {
                    tag: from,
                    missed: 0,
                };
                None
            }

            // A third UID. Neither candidate has been seen twice, so this is
            // noise rather than the start of anything.
            (State::Swapping { from, missed, .. }, Some(now)) => {
                self.state = State::Swapping {
                    from,
                    missed,
                    to: now,
                    seen: 1,
                };
                self.settle_swap(now, 1)
            }

            // Silence while a newcomer was pending. The figure that is still
            // believed present carries on toward its own departure, so noise
            // cannot keep a story alive after its figure has been lifted.
            (State::Swapping { from, missed, .. }, None) => {
                let missed = missed.saturating_add(1);
                if missed >= self.misses_to_leave {
                    self.state = State::Empty;
                    Some(TagEvent::Left)
                } else {
                    self.state = State::Present { tag: from, missed };
                    None
                }
            }
        }
    }

    /// Announces the departure once a newcomer has been seen often enough.
    ///
    /// The newcomer then earns its arrival from zero, exactly as it would have
    /// on an empty plate: the readings that proved the swap are spent on the
    /// departure.
    fn settle_swap(&mut self, to: TagUid, seen: u8) -> Option<TagEvent> {
        if seen >= self.arrivals_to_agree {
            self.state = State::Arriving { tag: to, seen: 0 };
            Some(TagEvent::Left)
        } else {
            None
        }
    }

    fn settle_arrival(&mut self, tag: TagUid, seen: u8) -> Option<TagEvent> {
        if seen >= self.arrivals_to_agree {
            self.state = State::Present { tag, missed: 0 };
            Some(TagEvent::Arrived(tag))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: TagUid = TagUid([1, 2, 3, 4, 5, 6, 7, 8]);
    const B: TagUid = TagUid([9, 9, 9, 9, 9, 9, 9, 9]);

    /// The module's own rule — "a single stray read must not start one" — has
    /// to hold for stopping too. Measured on the bench 2026-09-07: under radio
    /// traffic the reader occasionally answers with a UID that was never on the
    /// plate, and one such reading was enough to stop a story and restart it
    /// from the beginning.
    #[test]
    fn a_single_stray_reading_of_another_tag_does_not_end_the_story() {
        let mut p = Presence::new(2, 4);
        p.feed(Some(A));
        assert_eq!(p.feed(Some(A)), Some(TagEvent::Arrived(A)));

        assert_eq!(p.feed(Some(B)), None, "one reading is not a swap");
        assert_eq!(p.feed(Some(A)), None, "the figure never left");
    }

    /// Two different wrong readings in a row are two pieces of noise, not the
    /// beginning of a swap: neither has been seen twice.
    #[test]
    fn disagreeing_stray_readings_do_not_add_up_to_a_swap() {
        const C: TagUid = TagUid([7, 7, 7, 7, 7, 7, 7, 7]);
        let mut p = Presence::new(2, 4);
        p.feed(Some(A));
        p.feed(Some(A));

        assert_eq!(p.feed(Some(B)), None);
        assert_eq!(p.feed(Some(C)), None);
        assert_eq!(p.feed(Some(A)), None, "still the same figure throughout");
    }

    /// A figure lifted while a stray reading is pending still departs on the
    /// misses, so noise cannot keep a story alive after its figure is gone.
    #[test]
    fn a_lift_during_a_stray_reading_still_ends_the_story() {
        let mut p = Presence::new(2, 4);
        p.feed(Some(A));
        p.feed(Some(A));

        assert_eq!(p.feed(Some(B)), None);
        assert_eq!(p.feed(None), None);
        assert_eq!(p.feed(None), None);
        assert_eq!(p.feed(None), None);
        assert_eq!(p.feed(None), Some(TagEvent::Left));
    }

    /// Two agreeing readings, and not one before them.
    #[test]
    fn a_tag_arrives_only_once_its_readings_agree() {
        let mut p = Presence::new(2, 4);
        assert_eq!(p.feed(Some(A)), None);
        assert_eq!(p.feed(Some(A)), Some(TagEvent::Arrived(A)));
    }

    #[test]
    fn an_arrival_is_announced_once_not_on_every_reading() {
        let mut p = Presence::new(2, 4);
        p.feed(Some(A));
        p.feed(Some(A));
        assert_eq!(p.feed(Some(A)), None);
        assert_eq!(p.feed(Some(A)), None);
    }

    /// A stray reading shorter than the threshold is not a placement.
    #[test]
    fn a_flicker_below_the_threshold_produces_nothing() {
        let mut p = Presence::new(2, 4);
        assert_eq!(p.feed(Some(A)), None);
        assert_eq!(p.feed(None), None);
        assert_eq!(p.feed(Some(A)), None);
        assert_eq!(p.feed(None), None);
    }

    /// The case that matters most: a tag that reads intermittently is still
    /// on the plate, and stopping its story would be the visible failure.
    #[test]
    fn a_tag_that_reads_three_times_in_five_stays_present() {
        let mut p = Presence::new(2, 4);
        p.feed(Some(A));
        assert_eq!(p.feed(Some(A)), Some(TagEvent::Arrived(A)));
        assert_eq!(p.feed(None), None);
        assert_eq!(p.feed(Some(A)), None);
        assert_eq!(p.feed(None), None);
        assert_eq!(p.feed(None), None);
        assert_eq!(p.feed(Some(A)), None);
    }

    #[test]
    fn four_consecutive_misses_are_a_departure() {
        let mut p = Presence::new(2, 4);
        p.feed(Some(A));
        p.feed(Some(A));
        assert_eq!(p.feed(None), None);
        assert_eq!(p.feed(None), None);
        assert_eq!(p.feed(None), None);
        assert_eq!(p.feed(None), Some(TagEvent::Left));
    }

    #[test]
    fn a_departure_is_announced_once() {
        let mut p = Presence::new(2, 4);
        p.feed(Some(A));
        p.feed(Some(A));
        for _ in 0..4 {
            p.feed(None);
        }
        assert_eq!(p.feed(None), None);
    }

    /// Swapping figures without a gap must not look like one long presence —
    /// and the departure now has to be agreed, so the first sight of the
    /// newcomer announces nothing.
    #[test]
    fn swapping_one_figure_for_another_leaves_before_it_arrives() {
        let mut p = Presence::new(2, 4);
        p.feed(Some(A));
        assert_eq!(p.feed(Some(A)), Some(TagEvent::Arrived(A)));
        assert_eq!(p.feed(Some(B)), None);
        assert_eq!(p.feed(Some(B)), Some(TagEvent::Left));
        assert_eq!(p.feed(Some(B)), None);
        assert_eq!(p.feed(Some(B)), Some(TagEvent::Arrived(B)));
    }

    #[test]
    fn nothing_on_an_empty_plate_is_not_a_departure() {
        let mut p = Presence::new(2, 4);
        assert_eq!(p.feed(None), None);
        assert_eq!(p.feed(None), None);
        assert_eq!(p.feed(None), None);
        assert_eq!(p.feed(None), None);
        assert_eq!(p.feed(None), None);
    }

    /// A figure that displaces another mid-Arriving counts the displacing
    /// reading toward its own tally, since no departure is announced.
    #[test]
    fn a_figure_swapped_before_the_first_one_settled_starts_its_own_count() {
        let mut p = Presence::new(2, 4);
        p.feed(Some(A)); // A begins arriving but hasn't confirmed
        assert_eq!(p.feed(Some(B)), None); // B displaces mid-arriving A
        assert_eq!(p.feed(Some(B)), Some(TagEvent::Arrived(B))); // B arrives on second reading
    }
}
