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

use crate::{Event, TagUid};

/// Consecutive readings of the same tag before it counts as arrived.
///
/// Provisional. Nothing has calibrated this: it trades how fast a placement
/// feels against a false arrival. The precedent is
/// [`BatteryConfig::readings_to_agree`](crate::BatteryConfig::readings_to_agree),
/// which is 4 for a quantity that changes far more slowly than a hand.
pub const ARRIVALS_TO_AGREE: u8 = 2;

/// Consecutive empty readings before a tag counts as gone.
///
/// Provisional, and deliberately larger than `ARRIVALS_TO_AGREE`: stopping a
/// story that should still be playing is a worse failure than starting one
/// slightly late.
pub const MISSES_TO_LEAVE: u8 = 4;

/// How often the plate is read, empty or holding a figure.
///
/// Four misses at this cadence is how long a lift takes to pause a story:
/// 800 ms, chosen at the bench. The reader's field is on for the
/// whole boot, so a faster cadence costs executor time rather than field
/// time — about 12 ms per empty poll and 7 ms per occupied one, measured the
/// same day.
///
/// Not a free parameter even so: every poll transmits into the field, this
/// pack has no protection circuit, and the one unexplained brownout in this
/// project happened while transmitting into a coupled tag.
pub const POLL_MS: u32 = 200;

/// How soon a first reading of a figure is read again.
///
/// The second reading is what guards against a corrupt one; waiting a whole
/// poll for it guards against nothing and cost 507 ms per placement.
pub const CONFIRM_POLL_MS: u32 = 20;

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

    /// How long the reader should wait before its next reading.
    ///
    /// Only a figure arriving on an empty plate is confirmed at once; every
    /// other state is looked at on the ordinary cadence, which with
    /// `misses_to_leave` also sets how long a lift takes to be noticed.
    pub fn poll_again_in_ms(&self) -> u32 {
        match self.state {
            State::Arriving { seen, .. } if seen > 0 => CONFIRM_POLL_MS,
            _ => POLL_MS,
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

    /// An empty plate is where every placement starts, so how often it is
    /// looked at is half of how long a placement takes to be noticed.
    #[test]
    fn an_empty_plate_is_looked_at_every_200_ms() {
        let p = Presence::new(2, 4);
        assert_eq!(p.poll_again_in_ms(), 200);
    }

    /// Measured 2026-09-24: waiting a whole poll for the second reading was
    /// 507 of the ~675 ms between first reading a figure and its story
    /// starting. The second reading is the protection; the wait is not.
    #[test]
    fn a_first_reading_is_confirmed_at_once() {
        let mut p = Presence::new(2, 4);
        p.feed(Some(A));
        assert_eq!(p.poll_again_in_ms(), 20);
    }

    #[test]
    fn a_figure_on_the_plate_is_looked_at_every_200_ms() {
        let mut p = Presence::new(2, 4);
        p.feed(Some(A));
        p.feed(Some(A));
        assert_eq!(p.poll_again_in_ms(), 200);
    }

    /// A swap is where a corrupt reading was measured, under radio traffic,
    /// so it is not hurried: a second reading taken at once is more likely to
    /// share whatever corrupted the first.
    #[test]
    fn a_possible_swap_is_not_confirmed_at_once() {
        let mut p = Presence::new(2, 4);
        p.feed(Some(A));
        p.feed(Some(A));
        p.feed(Some(B));
        assert_eq!(p.poll_again_in_ms(), 200);
    }

    /// Lifting a figure pauses its story, so this is how long a child waits
    /// for the box to react to a lift.
    #[test]
    fn a_lifted_figure_is_gone_after_800_ms_of_silence() {
        let mut p = Presence::new(2, 4);
        p.feed(Some(A));
        p.feed(Some(A));

        let mut silent_ms = 0;
        loop {
            silent_ms += p.poll_again_in_ms();
            if p.feed(None) == Some(TagEvent::Left) {
                break;
            }
        }
        assert_eq!(silent_ms, 800);
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

/// What the reader last said about the plate.
///
/// The token rides in the same variant as the uid rather than in a second
/// message, for the reason [`Placed`] exists: two messages can be read in
/// either order, and "this figure, that figure's token" would then have a
/// representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Seen {
    Figure {
        uid: [u8; 8],
        token: Option<[u8; 32]>,
    },
    Nothing,
}

/// Whether an answer that names a figure is about the one on the plate.
///
/// Three outcomes rather than an `Option`, because "a different figure is
/// here" and "the plate is empty" are not the same event to whoever asked:
/// the first is worth saying out loud, and the second is the harmless case
/// the identity check exists to guarantee.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answering {
    TheFigure(TagUid),
    AnotherFigure,
    NoFigure,
}

/// The figure on the plate and the token that authorises fetching its story.
///
/// The two are one value because they are two halves of one identity, and
/// letting them drift apart is a real defect rather than a hypothetical one: a
/// token that outlives the figure it arrived with is a token available to
/// authorise the *next* figure's fetch. Every write goes through
/// [`Placed::observe`], so there is one place where they can disagree and it
/// replaces both.
///
/// Held by whoever drains the reader's signal, and asked — rather than read —
/// whenever a late answer has to be matched against what is here now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placed {
    figure: Option<TagUid>,
    token: Option<[u8; 32]>,
}

impl Default for Placed {
    fn default() -> Self {
        Self::empty()
    }
}

impl Placed {
    pub const fn empty() -> Self {
        Self {
            figure: None,
            token: None,
        }
    }

    /// Takes a reading and says what happened in the reducer's words.
    ///
    /// Total: every reading is a `TagPresent` or a `TagAbsent`, because the
    /// caller only has a reading at all when the reader's own filter has
    /// already decided something changed.
    pub fn observe(&mut self, seen: Seen) -> Event {
        match seen {
            Seen::Figure { uid, token } => {
                let tag = TagUid(uid);
                self.figure = Some(tag);
                self.token = token;
                Event::TagPresent(tag)
            }
            Seen::Nothing => {
                self.figure = None;
                self.token = None;
                Event::TagAbsent
            }
        }
    }

    pub fn figure(&self) -> Option<TagUid> {
        self.figure
    }

    /// The token, for whoever is about to authorise a fetch with it.
    pub fn token(&self) -> Option<[u8; 32]> {
        self.token
    }

    /// Whether an answer naming `ruid` belongs to what is on the plate now.
    ///
    /// The reducer checks the same identity again before it acts on the event
    /// this produces. That is not redundant: this decides whether the reducer
    /// is told at all, which is what keeps one figure's answer from ever being
    /// spoken about another.
    pub fn answering(&self, ruid: u64) -> Answering {
        match self.figure {
            Some(tag) if tag.ruid() == ruid => Answering::TheFigure(tag),
            Some(_) => Answering::AnotherFigure,
            None => Answering::NoFigure,
        }
    }
}

#[cfg(test)]
mod placed_tests {
    use super::*;

    const A: TagUid = TagUid([1, 2, 3, 4, 5, 6, 7, 8]);
    const B: TagUid = TagUid([9, 9, 9, 9, 9, 9, 9, 9]);

    const TOKEN_A: [u8; 32] = [0xAA; 32];
    const TOKEN_B: [u8; 32] = [0xBB; 32];

    fn figure(tag: TagUid, token: Option<[u8; 32]>) -> Seen {
        Seen::Figure { uid: tag.0, token }
    }

    #[test]
    fn a_figure_arriving_is_announced_and_its_token_kept() {
        let mut placed = Placed::empty();

        assert_eq!(
            placed.observe(figure(A, Some(TOKEN_A))),
            Event::TagPresent(A)
        );
        assert_eq!(placed.figure(), Some(A));
        assert_eq!(placed.token(), Some(TOKEN_A));
    }

    /// A figure whose token could not be read is still a figure. The box plays
    /// what the card already holds; only a fetch needs the token.
    #[test]
    fn a_figure_whose_token_was_not_read_is_still_announced() {
        let mut placed = Placed::empty();

        assert_eq!(placed.observe(figure(A, None)), Event::TagPresent(A));
        assert_eq!(placed.figure(), Some(A));
        assert_eq!(placed.token(), None);
    }

    /// The pair is the point. A token outliving the figure it came with is a
    /// token available to authorise the *next* figure's fetch.
    #[test]
    fn a_figure_leaving_takes_its_token_with_it() {
        let mut placed = Placed::empty();
        placed.observe(figure(A, Some(TOKEN_A)));

        assert_eq!(placed.observe(Seen::Nothing), Event::TagAbsent);
        assert_eq!(placed.figure(), None);
        assert_eq!(placed.token(), None);
    }

    /// The same rule across a swap, which is where it is easiest to get wrong:
    /// both halves move at once or neither does.
    #[test]
    fn a_replacing_figure_brings_its_own_token_and_not_the_last_one() {
        let mut placed = Placed::empty();
        placed.observe(figure(A, Some(TOKEN_A)));

        assert_eq!(
            placed.observe(figure(B, Some(TOKEN_B))),
            Event::TagPresent(B)
        );
        assert_eq!(placed.figure(), Some(B));
        assert_eq!(placed.token(), Some(TOKEN_B));
    }

    #[test]
    fn a_figure_replacing_one_that_had_a_token_does_not_inherit_it() {
        let mut placed = Placed::empty();
        placed.observe(figure(A, Some(TOKEN_A)));

        placed.observe(figure(B, None));
        assert_eq!(placed.token(), None, "B has no token of its own");
    }

    #[test]
    fn an_answer_naming_the_figure_on_the_plate_is_about_it() {
        let mut placed = Placed::empty();
        placed.observe(figure(A, Some(TOKEN_A)));

        assert_eq!(placed.answering(A.ruid()), Answering::TheFigure(A));
    }

    /// A console `get` that finishes while something else sits on the plate.
    /// Attributing it to whatever is there is the bug this identity exists to
    /// prevent.
    #[test]
    fn an_answer_naming_a_different_figure_is_not_about_the_one_on_the_plate() {
        let mut placed = Placed::empty();
        placed.observe(figure(A, Some(TOKEN_A)));

        assert_eq!(placed.answering(B.ruid()), Answering::AnotherFigure);
    }

    #[test]
    fn an_answer_arriving_at_an_empty_plate_is_about_nothing() {
        let placed = Placed::empty();

        assert_eq!(placed.answering(A.ruid()), Answering::NoFigure);
    }
}
