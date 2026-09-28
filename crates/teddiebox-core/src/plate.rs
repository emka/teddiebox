//! Decides whether a figure is on the plate, from readings that are sometimes
//! wrong.
//!
//! A tag can go unread even when nobody touched it, so one missed reading must
//! not stop a story, and one stray reading must not start one.
//!
//! A reading can also be wrong rather than missing: while Wi-Fi is busy, the
//! reader sometimes returns a UID that was never on the plate. So every change
//! needs several readings that agree — an arrival, a departure by absence, and
//! a departure because another figure replaced it.

use crate::{Event, TagUid, Unavailable};

/// Consecutive readings of the same tag before it counts as arrived.
///
/// Not calibrated: it trades how fast a placement feels against the risk of a
/// false arrival. Compare
/// [`BatteryConfig::readings_to_agree`](crate::BatteryConfig::readings_to_agree),
/// which is 4 for a value that changes far more slowly.
pub const ARRIVALS_TO_AGREE: u8 = 2;

/// Consecutive empty readings before a tag counts as gone.
///
/// Not calibrated. Larger than `ARRIVALS_TO_AGREE` on purpose: stopping a
/// story that should keep playing is worse than starting one a little late.
pub const MISSES_TO_LEAVE: u8 = 4;

/// How often an empty plate is read.
///
/// This sets about half of how long a placement takes to be noticed. The
/// reader's field is always on, so polling faster costs CPU time (about 12 ms
/// per empty poll), not extra field time.
///
/// Still, do not lower it freely: every poll transmits, the battery pack has
/// no protection circuit, and a brownout has been seen while transmitting to
/// a tag.
pub const EMPTY_POLL_MS: u32 = 200;

/// How often a plate holding a figure is read.
///
/// Slower than an empty plate, because a figure on the plate usually means a
/// story is playing, and each poll can disturb the audio. Polling every
/// 200 ms caused 12 audible glitches in 49 s of playback; every 500 ms caused
/// 4 in 47 s. How fast a lift is noticed is set by [`LEAVING_POLL_MS`].
pub const OCCUPIED_POLL_MS: u32 = 500;

/// How soon a figure that missed a reading is read again.
///
/// A lift always starts with a miss, so after a miss the plate is read
/// quickly, and the audio is disturbed only then. A lift is noticed after one
/// occupied poll plus three of these, instead of four occupied polls.
///
/// Not as short as [`CONFIRM_POLL_MS`]: while Wi-Fi is busy the reader can
/// miss a figure that did not move, and readings taken back to back are more
/// likely to fail for the same reason. 100 ms has not been tested with Wi-Fi
/// running.
pub const LEAVING_POLL_MS: u32 = 100;

/// How soon a first reading of a figure is read again.
///
/// The second reading protects against a corrupt first one. Waiting a whole
/// poll for it adds nothing and made each placement about 500 ms slower.
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

    /// Takes one poll's result and returns an event once the readings agree.
    /// When a different tag replaces the current one, the departure is
    /// reported first and the new arrival on a later call.
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

            // A different tag restarts the count. Nothing had arrived yet, so
            // there is no departure; this reading counts for the new tag.
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

            // Another figure while one is present. Do not report a departure
            // yet: a swap needs agreeing readings like an arrival does, or
            // one corrupt reading would stop the story.
            (State::Present { tag, missed }, Some(now)) => {
                self.state = State::Swapping {
                    from: tag,
                    missed,
                    to: now,
                    seen: 1,
                };
                self.settle_swap(now, 1)
            }

            // The new figure again. Once enough readings agree, the old
            // figure has really gone.
            (State::Swapping { to, seen, .. }, Some(now)) if now == to => {
                let seen = seen.saturating_add(1);
                self.settle_swap(to, seen)
            }

            // The original figure answered, so the odd reading was noise.
            // Reset the miss count too: this was a reading, not a miss.
            (State::Swapping { from, .. }, Some(now)) if now == from => {
                self.state = State::Present {
                    tag: from,
                    missed: 0,
                };
                None
            }

            // A third UID. Neither new UID has been seen twice, so this is
            // noise.
            (State::Swapping { from, missed, .. }, Some(now)) => {
                self.state = State::Swapping {
                    from,
                    missed,
                    to: now,
                    seen: 1,
                };
                self.settle_swap(now, 1)
            }

            // A miss while a new figure is pending. Count it against the
            // current figure, so noise cannot keep a story playing after its
            // figure was lifted.
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
    /// A new figure on an empty plate is confirmed at once. A plate holding a
    /// figure (even one that may be being swapped) is read slowly until a
    /// reading misses, then quickly until the figure answers or is gone.
    pub fn poll_again_in_ms(&self) -> u32 {
        match self.state {
            State::Arriving { seen, .. } if seen > 0 => CONFIRM_POLL_MS,
            State::Empty | State::Arriving { .. } => EMPTY_POLL_MS,
            State::Present { missed, .. } if missed > 0 => LEAVING_POLL_MS,
            State::Present { .. } | State::Swapping { .. } => OCCUPIED_POLL_MS,
        }
    }

    /// Reports the departure once the new figure has been seen often enough.
    ///
    /// The new figure then starts counting its arrival from zero, as it would
    /// on an empty plate: the readings that proved the swap are used up on
    /// the departure.
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

    /// One stray reading must not stop a story. While Wi-Fi is busy, the
    /// reader sometimes returns a UID that was never on the plate.
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

    /// Swapping figures without a gap must not look like one long presence.
    /// The departure needs agreeing readings, so the first reading of the new
    /// figure reports nothing.
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

    /// Every placement starts on an empty plate, so this sets about half of
    /// how long a placement takes to be noticed.
    #[test]
    fn an_empty_plate_is_looked_at_every_200_ms() {
        let p = Presence::new(2, 4);
        assert_eq!(p.poll_again_in_ms(), 200);
    }

    /// Waiting a whole poll for the second reading took about 500 of the
    /// ~675 ms between first seeing a figure and its story starting. The
    /// second reading is what protects; the wait is not.
    #[test]
    fn a_first_reading_is_confirmed_at_once() {
        let mut p = Presence::new(2, 4);
        p.feed(Some(A));
        assert_eq!(p.poll_again_in_ms(), 20);
    }

    /// A figure on the plate usually means a story is playing. Reading it
    /// every 200 ms caused 12 audible glitches in 49 s of playback; every
    /// 500 ms caused 4 in 47 s.
    #[test]
    fn a_figure_on_the_plate_is_looked_at_every_500_ms() {
        let mut p = Presence::new(2, 4);
        p.feed(Some(A));
        p.feed(Some(A));
        assert_eq!(p.poll_again_in_ms(), 500);
    }

    /// Corrupt readings that look like a swap happen while Wi-Fi is busy, so a
    /// swap is not confirmed at once: a second reading taken straight away is
    /// more likely to be corrupt for the same reason.
    #[test]
    fn a_possible_swap_is_not_confirmed_at_once() {
        let mut p = Presence::new(2, 4);
        p.feed(Some(A));
        p.feed(Some(A));
        p.feed(Some(B));
        assert_eq!(p.poll_again_in_ms(), 500);
    }

    /// Lifting a figure pauses its story, so this is how long a child waits
    /// for the box to react to a lift: one ordinary poll to notice the
    /// silence, then three quick ones to agree it.
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

    /// A lift always starts with a miss, so that is when the plate is read
    /// quickly. Reading quickly all through a story causes audible glitches.
    #[test]
    fn a_missed_figure_is_looked_at_again_after_100_ms() {
        let mut p = Presence::new(2, 4);
        p.feed(Some(A));
        p.feed(Some(A));
        p.feed(None);
        assert_eq!(p.poll_again_in_ms(), 100);
    }

    /// A figure that answers after a miss was never lifted, so polling goes
    /// back to the slow rate.
    #[test]
    fn a_figure_answering_after_a_miss_is_looked_at_every_500_ms_again() {
        let mut p = Presence::new(2, 4);
        p.feed(Some(A));
        p.feed(Some(A));
        p.feed(None);
        p.feed(Some(A));
        assert_eq!(p.poll_again_in_ms(), 500);
    }

    /// A figure that replaces one not yet confirmed counts that reading
    /// toward its own arrival, since there is no departure to report.
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
/// The token travels with the uid in one value, not in a separate message:
/// two messages could be read in either order, and a figure could end up
/// paired with another figure's token. See [`Placed`].
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
/// Three outcomes instead of an `Option`, because "a different figure is
/// here" and "the plate is empty" are handled differently: the first is worth
/// telling the user about, the second is harmless.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answering {
    TheFigure(TagUid),
    AnotherFigure,
    NoFigure,
}

/// The figure on the plate and the token that authorises fetching its story.
///
/// Kept together because a token that outlives its figure could authorise a
/// fetch for the *next* figure. Every change goes through
/// [`Placed::observe`], which always replaces both.
///
/// Owned by the task that receives reader updates. Use [`Placed::answering`]
/// to check whether a late answer still belongs to the figure on the plate.
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

    /// Records a reading and returns it as an [`Event`] for the reducer.
    ///
    /// Always returns an event: the caller only gets a reading after the
    /// reader's filter has decided something changed.
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
    /// The reducer checks the identity again before acting. This check decides
    /// whether the reducer is told at all, so one figure's answer is never
    /// reported for another.
    pub fn answering(&self, ruid: u64) -> Answering {
        match self.figure {
            Some(tag) if tag.ruid() == ruid => Answering::TheFigure(tag),
            Some(_) => Answering::AnotherFigure,
            None => Answering::NoFigure,
        }
    }
}

/// What a finished download means for the reducer, once it is known which
/// figure (if any) it was for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Settlement {
    /// The figure it was for is still on the plate.
    ForTheFigure(Event),
    /// A different figure is on the plate now, for example after a console
    /// `get`. Worth telling the user about; not passed to the reducer.
    ForAnotherFigure,
    /// Nobody is waiting; nothing to do.
    NothingWaiting,
}

/// Turns a finished download's outcome into what the reducer should be told,
/// given whose figure it was for.
pub fn settle_fetch_outcome(
    answering: Answering,
    outcome: teddiebox_download::Outcome,
) -> Settlement {
    use teddiebox_download::Outcome;
    match answering {
        Answering::TheFigure(tag) => Settlement::ForTheFigure(match outcome {
            Outcome::Completed => Event::ContentReady(tag),
            Outcome::Unreachable => Event::ContentMissing(tag, Unavailable::Unreachable),
            Outcome::NoContent => Event::ContentMissing(tag, Unavailable::NoContent),
            Outcome::Refused => Event::ContentMissing(tag, Unavailable::Refused),
        }),
        Answering::AnotherFigure => Settlement::ForAnotherFigure,
        Answering::NoFigure => Settlement::NothingWaiting,
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

    /// A figure whose token could not be read is still a figure. The box can
    /// play what the card already holds; only a download needs the token.
    #[test]
    fn a_figure_whose_token_was_not_read_is_still_announced() {
        let mut placed = Placed::empty();

        assert_eq!(placed.observe(figure(A, None)), Event::TagPresent(A));
        assert_eq!(placed.figure(), Some(A));
        assert_eq!(placed.token(), None);
    }

    /// A token that outlives its figure could authorise the *next* figure's
    /// download.
    #[test]
    fn a_figure_leaving_takes_its_token_with_it() {
        let mut placed = Placed::empty();
        placed.observe(figure(A, Some(TOKEN_A)));

        assert_eq!(placed.observe(Seen::Nothing), Event::TagAbsent);
        assert_eq!(placed.figure(), None);
        assert_eq!(placed.token(), None);
    }

    /// The same rule during a swap: figure and token change together.
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

    /// A console `get` that finishes while another figure is on the plate
    /// must not be reported for that figure.
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

#[cfg(test)]
mod settle_fetch_outcome_tests {
    use super::*;
    use teddiebox_download::Outcome;

    const A: TagUid = TagUid([1, 2, 3, 4, 5, 6, 7, 8]);

    #[test]
    fn a_completed_download_for_the_figure_on_the_plate_makes_content_ready() {
        assert_eq!(
            settle_fetch_outcome(Answering::TheFigure(A), Outcome::Completed),
            Settlement::ForTheFigure(Event::ContentReady(A))
        );
    }

    #[test]
    fn an_unreachable_server_for_the_figure_on_the_plate_reports_why() {
        assert_eq!(
            settle_fetch_outcome(Answering::TheFigure(A), Outcome::Unreachable),
            Settlement::ForTheFigure(Event::ContentMissing(A, Unavailable::Unreachable))
        );
    }

    #[test]
    fn no_content_for_the_figure_on_the_plate_reports_why() {
        assert_eq!(
            settle_fetch_outcome(Answering::TheFigure(A), Outcome::NoContent),
            Settlement::ForTheFigure(Event::ContentMissing(A, Unavailable::NoContent))
        );
    }

    #[test]
    fn a_refused_join_for_the_figure_on_the_plate_reports_why() {
        assert_eq!(
            settle_fetch_outcome(Answering::TheFigure(A), Outcome::Refused),
            Settlement::ForTheFigure(Event::ContentMissing(A, Unavailable::Refused))
        );
    }

    /// A console `get` that finished while another figure replaced the one it
    /// was fetching for must not be reported for that figure.
    #[test]
    fn an_outcome_for_another_figure_is_not_settled_against_the_one_on_the_plate() {
        assert_eq!(
            settle_fetch_outcome(Answering::AnotherFigure, Outcome::Completed),
            Settlement::ForAnotherFigure
        );
    }

    #[test]
    fn an_outcome_with_nobody_waiting_settles_nothing() {
        assert_eq!(
            settle_fetch_outcome(Answering::NoFigure, Outcome::Completed),
            Settlement::NothingWaiting
        );
    }
}
