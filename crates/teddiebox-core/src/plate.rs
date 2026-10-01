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
/// Still, do not lower it freely: every poll transmits, the battery has
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

    fn presence() -> Presence {
        Presence::new(2, 4)
    }

    /// A plate on which `tag` has arrived.
    fn holding(tag: TagUid) -> Presence {
        let mut p = presence();
        p.feed(Some(tag));
        assert_eq!(p.feed(Some(tag)), Some(TagEvent::Arrived(tag)));
        p
    }

    /// One stray reading must not stop a story. While Wi-Fi is busy, the
    /// reader sometimes returns a UID that was never on the plate.
    #[test]
    fn a_single_stray_reading_of_another_tag_does_not_end_the_story() {
        // Given
        let mut p = holding(A);

        // When: one reading is not a swap, and the figure never left
        let events = [Some(B), Some(A), Some(A)].map(|seen| p.feed(seen));

        // Then
        assert_eq!(events, [None, None, None]);
    }

    /// Two different wrong readings in a row are two pieces of noise, not the
    /// beginning of a swap: neither has been seen twice.
    #[test]
    fn disagreeing_stray_readings_do_not_add_up_to_a_swap() {
        // Given
        const C: TagUid = TagUid([7, 7, 7, 7, 7, 7, 7, 7]);
        let mut p = holding(A);

        // When
        let events = [Some(B), Some(C), Some(A)].map(|seen| p.feed(seen));

        // Then
        assert_eq!(
            events,
            [None, None, None],
            "still the same figure throughout"
        );
    }

    /// Only the stray reading is noise. A newcomer that goes on answering after
    /// it is a real swap, however it began.
    #[test]
    fn a_newcomer_seen_twice_after_a_stray_reading_is_a_swap() {
        // Given
        const C: TagUid = TagUid([7, 7, 7, 7, 7, 7, 7, 7]);
        let mut p = holding(A);

        // When
        let events = [Some(B), Some(C), Some(C)].map(|seen| p.feed(seen));

        // Then
        assert_eq!(events, [None, None, Some(TagEvent::Left)]);
    }

    /// A figure lifted while a stray reading is pending still departs on the
    /// misses, so noise cannot keep a story alive after its figure is gone.
    #[test]
    fn a_lift_during_a_stray_reading_still_ends_the_story() {
        // Given
        let mut p = holding(A);

        // When
        let events = [Some(B), None, None, None, None].map(|seen| p.feed(seen));

        // Then
        assert_eq!(events, [None, None, None, None, Some(TagEvent::Left)]);
    }

    /// Two agreeing readings, and not one before them.
    #[test]
    fn a_tag_arrives_only_once_its_readings_agree() {
        // Given
        let mut p = presence();

        // When
        let events = [Some(A), Some(A)].map(|seen| p.feed(seen));

        // Then
        assert_eq!(events, [None, Some(TagEvent::Arrived(A))]);
    }

    #[test]
    fn an_arrival_is_announced_once_not_on_every_reading() {
        // Given
        let mut p = holding(A);

        // When
        let events = [Some(A), Some(A)].map(|seen| p.feed(seen));

        // Then
        assert_eq!(events, [None, None]);
    }

    /// A stray reading shorter than the threshold is not a placement.
    #[test]
    fn a_flicker_below_the_threshold_produces_nothing() {
        // Given
        let mut p = presence();

        // When
        let events = [Some(A), None, Some(A), None].map(|seen| p.feed(seen));

        // Then
        assert_eq!(events, [None; 4]);
    }

    /// The case that matters most: a tag that reads intermittently is still
    /// on the plate, and stopping its story would be the visible failure.
    #[test]
    fn a_tag_that_reads_three_times_in_five_stays_present() {
        // Given: the first two of the five readings, which made it arrive
        let mut p = holding(A);

        // When
        let events = [None, Some(A), None, None, Some(A)].map(|seen| p.feed(seen));

        // Then
        assert_eq!(events, [None; 5]);
    }

    #[test]
    fn four_consecutive_misses_are_a_departure() {
        // Given
        let mut p = holding(A);

        // When
        let events = [None; 4].map(|seen| p.feed(seen));

        // Then
        assert_eq!(events, [None, None, None, Some(TagEvent::Left)]);
    }

    #[test]
    fn a_departure_is_announced_once() {
        // Given
        let mut p = holding(A);
        for _ in 0..4 {
            p.feed(None);
        }

        // When
        let event = p.feed(None);

        // Then
        assert_eq!(event, None);
    }

    /// Swapping figures without a gap must not look like one long presence.
    /// The departure needs agreeing readings, so the first reading of the new
    /// figure reports nothing.
    #[test]
    fn swapping_one_figure_for_another_leaves_before_it_arrives() {
        // Given
        let mut p = holding(A);

        // When
        let events = [Some(B); 4].map(|seen| p.feed(seen));

        // Then
        assert_eq!(
            events,
            [None, Some(TagEvent::Left), None, Some(TagEvent::Arrived(B))]
        );
    }

    #[test]
    fn nothing_on_an_empty_plate_is_not_a_departure() {
        // Given
        let mut p = presence();

        // When
        let events = [None; 5].map(|seen| p.feed(seen));

        // Then
        assert_eq!(events, [None; 5]);
    }

    /// Every placement starts on an empty plate, so this sets about half of
    /// how long a placement takes to be noticed.
    #[test]
    fn an_empty_plate_is_looked_at_every_200_ms() {
        // Given
        let p = presence();

        // When
        let wait = p.poll_again_in_ms();

        // Then
        assert_eq!(wait, 200);
    }

    /// Waiting a whole poll for the second reading took about 500 of the
    /// ~675 ms between first seeing a figure and its story starting. The
    /// second reading is what protects; the wait is not.
    #[test]
    fn a_first_reading_is_confirmed_at_once() {
        // Given
        let mut p = presence();

        // When
        p.feed(Some(A));

        // Then
        assert_eq!(p.poll_again_in_ms(), 20);
    }

    /// A figure on the plate usually means a story is playing. Reading it
    /// every 200 ms caused 12 audible glitches in 49 s of playback; every
    /// 500 ms caused 4 in 47 s.
    #[test]
    fn a_figure_on_the_plate_is_looked_at_every_500_ms() {
        // Given
        let p = holding(A);

        // When
        let wait = p.poll_again_in_ms();

        // Then
        assert_eq!(wait, 500);
    }

    /// Corrupt readings that look like a swap happen while Wi-Fi is busy, so a
    /// swap is not confirmed at once: a second reading taken straight away is
    /// more likely to be corrupt for the same reason.
    #[test]
    fn a_possible_swap_is_not_confirmed_at_once() {
        // Given
        let mut p = holding(A);

        // When
        p.feed(Some(B));

        // Then
        assert_eq!(p.poll_again_in_ms(), 500);
    }

    /// The readings that proved a swap are used up on the departure, so the
    /// newcomer starts as if just placed on an empty plate. Nothing of it has
    /// been read yet that a quick second reading could confirm.
    #[test]
    fn after_a_swap_the_newcomer_is_looked_at_like_an_empty_plate() {
        // Given: A replaced by B
        let mut p = holding(A);
        p.feed(Some(B));
        assert_eq!(p.feed(Some(B)), Some(TagEvent::Left));

        // When
        let wait = p.poll_again_in_ms();

        // Then
        assert_eq!(wait, 200);
    }

    /// Lifting a figure pauses its story, so this is how long a child waits
    /// for the box to react to a lift: one ordinary poll to notice the
    /// silence, then three quick ones to agree it.
    #[test]
    fn a_lifted_figure_is_gone_after_800_ms_of_silence() {
        // Given
        let mut p = holding(A);

        // When
        let mut silent_ms = 0;
        loop {
            silent_ms += p.poll_again_in_ms();
            if p.feed(None) == Some(TagEvent::Left) {
                break;
            }
        }

        // Then
        assert_eq!(silent_ms, 800);
    }

    /// A lift always starts with a miss, so that is when the plate is read
    /// quickly. Reading quickly all through a story causes audible glitches.
    #[test]
    fn a_missed_figure_is_looked_at_again_after_100_ms() {
        // Given
        let mut p = holding(A);

        // When
        p.feed(None);

        // Then
        assert_eq!(p.poll_again_in_ms(), 100);
    }

    /// A figure that answers after a miss was never lifted, so polling goes
    /// back to the slow rate.
    #[test]
    fn a_figure_answering_after_a_miss_is_looked_at_every_500_ms_again() {
        // Given
        let mut p = holding(A);
        p.feed(None);

        // When
        p.feed(Some(A));

        // Then
        assert_eq!(p.poll_again_in_ms(), 500);
    }

    /// A figure that replaces one not yet confirmed counts that reading
    /// toward its own arrival, since there is no departure to report.
    #[test]
    fn a_figure_swapped_before_the_first_one_settled_starts_its_own_count() {
        // Given: A has begun arriving but has not been confirmed
        let mut p = presence();
        p.feed(Some(A));

        // When
        let events = [Some(B), Some(B)].map(|seen| p.feed(seen));

        // Then
        assert_eq!(events, [None, Some(TagEvent::Arrived(B))]);
    }
}

/// What deciding which reader call to make next needs from the NFC reader.
///
/// Narrow on purpose: `unlock`, `lock`, `dump_memory` and the rest are console
/// commands with no decision in them, wired straight to the real reader in
/// `firmware/`. Only the calls [`PlatePoll::poll`] chooses between are here,
/// so the choice can be exercised on the host against a fake.
pub trait PlateReader {
    /// Re-reads an already-identified figure. Answers only while it is still
    /// there and still unlocked.
    fn identify(&mut self) -> Option<[u8; 8]>;
    /// Whether anything at all answers the plate, unlocked or not.
    fn tag_present(&mut self) -> bool;
    /// Identifies whatever is on the plate, unlocking it with `password`
    /// first. The one call that sends the password, so it is only made when
    /// [`Self::tag_present`] found something to unlock.
    fn inventory_unlocked(&mut self, password: u32) -> Option<[u8; 8]>;
}

/// What one call to [`PlatePoll::poll`] found, once it actually polled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Polled {
    /// A change in what is on the plate, if the readings agree on one.
    pub event: Option<TagEvent>,
    /// Set when this poll answered after a run of misses: how long the run
    /// was, worth telling the console. `None` on a poll that already
    /// answered last time, so nothing interrupted.
    pub resumed_after_misses: Option<u16>,
    /// Consecutive polls that have found nothing, including this one.
    /// Zero whenever this poll answered; meaningful alongside
    /// `event == Some(TagEvent::Left)`, which only fires on a miss.
    pub misses_now: u16,
}

/// Polls the plate on the schedule [`Presence`] sets, picking the cheapest
/// reader call each time.
///
/// Owns everything the schedule depends on, so the caller only threads a
/// reader, whether polling is switched on, the password and the clock
/// through [`poll`](Self::poll).
pub struct PlatePoll {
    presence: Presence,
    arrivals_to_agree: u8,
    misses_to_leave: u8,
    was_polling: bool,
    believed_present: bool,
    misses: u16,
    next_poll_ms: u64,
}

impl PlatePoll {
    pub const fn new(arrivals_to_agree: u8, misses_to_leave: u8) -> Self {
        Self {
            presence: Presence::new(arrivals_to_agree, misses_to_leave),
            arrivals_to_agree,
            misses_to_leave,
            was_polling: false,
            believed_present: false,
            misses: 0,
            next_poll_ms: 0,
        }
    }

    /// When the plate is next due to be read, in the same clock as
    /// [`poll`](Self::poll)'s `now_ms`.
    ///
    /// Meaningless while polling is off: the caller decides how it schedules
    /// itself then.
    pub const fn next_poll_ms(&self) -> u64 {
        self.next_poll_ms
    }

    /// Polls `reader` if switching on or due, and reports what happened.
    ///
    /// `None` when nothing was asked of `reader` this call: polling is off,
    /// or the next poll is not due yet at `now_ms`.
    ///
    /// **Switching polling on forgets what the plate held.** [`Presence`]
    /// reports changes, not states, so a figure it already believed present
    /// would not be reported again — and while polling was off the figure
    /// may have left or been swapped. Turning polling on always starts
    /// fresh, whatever `reader` says on the first poll after.
    pub fn poll(
        &mut self,
        reader: &mut impl PlateReader,
        polling: bool,
        password: u32,
        now_ms: u64,
    ) -> Option<Polled> {
        if polling && !self.was_polling {
            self.presence = Presence::new(self.arrivals_to_agree, self.misses_to_leave);
            self.believed_present = false;
            self.misses = 0;
            self.next_poll_ms = now_ms;
        }
        self.was_polling = polling;

        if !polling || now_ms < self.next_poll_ms {
            return None;
        }

        // Which request is cheapest depends on what was there last time.
        // Always sending the full unlock caused 49 audio DMA restarts in
        // 70 s of playback, against 0 with polling off, because every
        // unanswered exchange blocks the reader task for the whole
        // IRQ_POLL_ATTEMPTS window.
        let seen = if self.believed_present {
            reader.identify()
        } else if reader.tag_present() {
            reader.inventory_unlocked(password)
        } else {
            None
        };

        let misses_before = self.misses;
        self.misses = if seen.is_none() {
            self.misses.saturating_add(1)
        } else {
            0
        };
        self.believed_present = seen.is_some();

        let event = self.presence.feed(seen.map(TagUid));
        self.next_poll_ms = now_ms + u64::from(self.presence.poll_again_in_ms());

        Some(Polled {
            event,
            resumed_after_misses: (seen.is_some() && misses_before > 0).then_some(misses_before),
            misses_now: self.misses,
        })
    }
}

#[cfg(test)]
mod plate_poll_tests {
    use super::*;

    const UID: [u8; 8] = [1, 2, 3, 4, 5, 6, 7, 8];
    const PASSWORD: u32 = 0xDEAD_BEEF;

    #[derive(Default)]
    struct FakeReader {
        identify: Option<[u8; 8]>,
        tag_present: bool,
        inventory_unlocked: Option<[u8; 8]>,
        identify_calls: u8,
        tag_present_calls: u8,
        inventory_unlocked_calls: u8,
        inventory_unlocked_password: Option<u32>,
    }

    impl PlateReader for FakeReader {
        fn identify(&mut self) -> Option<[u8; 8]> {
            self.identify_calls += 1;
            self.identify
        }

        fn tag_present(&mut self) -> bool {
            self.tag_present_calls += 1;
            self.tag_present
        }

        fn inventory_unlocked(&mut self, password: u32) -> Option<[u8; 8]> {
            self.inventory_unlocked_calls += 1;
            self.inventory_unlocked_password = Some(password);
            self.inventory_unlocked
        }
    }

    /// How often each call was made: `identify`, `tag_present`, then
    /// `inventory_unlocked`.
    fn calls(reader: &FakeReader) -> (u8, u8, u8) {
        (
            reader.identify_calls,
            reader.tag_present_calls,
            reader.inventory_unlocked_calls,
        )
    }

    /// A plate nothing answers.
    fn empty_plate() -> FakeReader {
        FakeReader::default()
    }

    /// A figure that answers whichever way it is asked: once the first poll
    /// finds it, later polls believe it present and switch to `identify`.
    fn figure_on_plate() -> FakeReader {
        FakeReader {
            tag_present: true,
            inventory_unlocked: Some(UID),
            identify: Some(UID),
            ..FakeReader::default()
        }
    }

    #[test]
    fn polling_off_never_touches_the_reader() {
        // Given
        let mut poll = PlatePoll::new(2, 4);
        let mut reader = figure_on_plate();

        // When
        let polled = poll.poll(&mut reader, false, PASSWORD, 0);

        // Then
        assert_eq!(polled, None);
        assert_eq!(calls(&reader), (0, 0, 0));
    }

    #[test]
    fn not_due_yet_returns_none_without_touching_the_reader() {
        // Given
        let mut poll = PlatePoll::new(2, 4);
        poll.poll(&mut empty_plate(), true, PASSWORD, 0);
        let mut reader = empty_plate();

        // When
        let polled = poll.poll(&mut reader, true, PASSWORD, 0);

        // Then
        assert_eq!(polled, None);
        assert_eq!(calls(&reader), (0, 0, 0));
    }

    /// The firmware sleeps until this time, so it sets how often the reader
    /// transmits.
    #[test]
    fn the_next_poll_is_due_one_interval_after_the_last() {
        // Given
        let mut poll = PlatePoll::new(2, 4);

        // When
        poll.poll(&mut empty_plate(), true, PASSWORD, 1_000);

        // Then
        assert_eq!(poll.next_poll_ms(), 1_200);
    }

    #[test]
    fn neither_present_nor_believed_polls_nothing_further() {
        // Given
        let mut poll = PlatePoll::new(2, 4);
        let mut reader = empty_plate();

        // When
        poll.poll(&mut reader, true, PASSWORD, 0);

        // Then
        assert_eq!(calls(&reader), (0, 1, 0));
    }

    #[test]
    fn a_figure_not_yet_believed_present_is_unlocked_with_the_password() {
        // Given
        let mut poll = PlatePoll::new(2, 4);
        let mut reader = figure_on_plate();

        // When
        poll.poll(&mut reader, true, PASSWORD, 0);

        // Then
        assert_eq!(calls(&reader), (0, 1, 1));
        assert_eq!(reader.inventory_unlocked_password, Some(PASSWORD));
    }

    #[test]
    fn a_figure_believed_present_is_only_re_identified() {
        // Given
        let mut poll = PlatePoll::new(2, 4);
        poll.poll(&mut figure_on_plate(), true, PASSWORD, 0);
        let mut reader = figure_on_plate();

        // When
        poll.poll(&mut reader, true, PASSWORD, 1000);

        // Then
        assert_eq!(calls(&reader), (1, 0, 0));
    }

    /// The documented reason polling has to reset on: without it, turning
    /// polling back on with the same figure still on the plate would poll it
    /// as already-believed-present forever, when a swap or a lift while
    /// polling was off is exactly what the next poll needs to notice.
    #[test]
    fn switching_polling_on_forgets_a_previously_believed_figure() {
        // Given: a figure believed present, then polling switched off
        let mut poll = PlatePoll::new(2, 4);
        poll.poll(&mut figure_on_plate(), true, PASSWORD, 0);
        assert_eq!(poll.poll(&mut empty_plate(), false, PASSWORD, 1000), None);
        let mut reader = empty_plate();

        // When
        poll.poll(&mut reader, true, PASSWORD, 2000);

        // Then
        assert_eq!(
            calls(&reader),
            (0, 1, 0),
            "a forgotten figure is asked for like a new one, not re-identified"
        );
    }

    #[test]
    fn a_miss_is_not_reported_until_the_plate_answers_again() {
        // Given
        let mut poll = PlatePoll::new(2, 4);

        // When
        let resumed = [
            (empty_plate(), 0),
            (empty_plate(), 1000),
            (figure_on_plate(), 2000),
        ]
        .map(|(mut reader, at)| {
            poll.poll(&mut reader, true, PASSWORD, at)
                .unwrap()
                .resumed_after_misses
        });

        // Then
        assert_eq!(resumed, [None, None, Some(2)]);
    }

    #[test]
    fn a_first_ever_answer_reports_no_resume() {
        // Given
        let mut poll = PlatePoll::new(2, 4);

        // When
        let polled = poll
            .poll(&mut figure_on_plate(), true, PASSWORD, 0)
            .unwrap();

        // Then
        assert_eq!(polled.resumed_after_misses, None);
    }

    #[test]
    fn an_arrival_needs_two_agreeing_reads_like_presence_alone() {
        // Given
        let mut poll = PlatePoll::new(2, 4);

        // When
        let events = [0, 20].map(|at| {
            poll.poll(&mut figure_on_plate(), true, PASSWORD, at)
                .unwrap()
                .event
        });

        // Then
        assert_eq!(events, [None, Some(TagEvent::Arrived(TagUid(UID)))]);
    }

    #[test]
    fn four_misses_leave_with_the_full_run_length() {
        // Given: a figure that has arrived
        let mut poll = PlatePoll::new(2, 4);
        poll.poll(&mut figure_on_plate(), true, PASSWORD, 0);
        poll.poll(&mut figure_on_plate(), true, PASSWORD, 20);

        // When
        let polls = [1000, 2000, 3000, 4000]
            .map(|at| poll.poll(&mut empty_plate(), true, PASSWORD, at).unwrap());

        // Then
        let last = polls[3];
        assert_eq!(last.event, Some(TagEvent::Left));
        assert_eq!(last.misses_now, 4);
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

/// What answering the plate a certain way means for the reducer, once it is
/// known whether the answer is still about the figure it was asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Settlement<T> {
    /// The figure it was about is still on the plate.
    ForTheFigure(T),
    /// A different figure is on the plate now, for example after a console
    /// `get`. Worth telling the user about; not passed to the reducer.
    ForAnotherFigure,
    /// Nobody is waiting; nothing to do.
    NothingWaiting,
}

impl Answering {
    /// Calls `settle` with the figure an answer was about, but only when that
    /// figure is still the one on the plate.
    ///
    /// Shared by every place that turns a late answer into a reducer event —
    /// a finished download, a revalidation — so which of the three cases
    /// applies is decided once, the same way, whatever the answer is.
    pub fn resolve<T>(self, settle: impl FnOnce(TagUid) -> T) -> Settlement<T> {
        match self {
            Answering::TheFigure(tag) => Settlement::ForTheFigure(settle(tag)),
            Answering::AnotherFigure => Settlement::ForAnotherFigure,
            Answering::NoFigure => Settlement::NothingWaiting,
        }
    }
}

/// Turns a finished download's outcome into what the reducer should be told,
/// given whose figure it was for.
pub fn settle_fetch_outcome(
    answering: Answering,
    outcome: teddiebox_download::Outcome,
) -> Settlement<Event> {
    use teddiebox_download::Outcome;
    answering.resolve(|tag| match outcome {
        Outcome::Completed => Event::ContentReady(tag),
        Outcome::Unreachable => Event::ContentMissing(tag, Unavailable::Unreachable),
        Outcome::NoContent => Event::ContentMissing(tag, Unavailable::NoContent),
        Outcome::Refused => Event::ContentMissing(tag, Unavailable::Refused),
    })
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
    fn a_figure_arriving_is_announced() {
        // Given
        let mut placed = Placed::empty();

        // When
        let event = placed.observe(figure(A, Some(TOKEN_A)));

        // Then
        assert_eq!(event, Event::TagPresent(A));
        assert_eq!(placed.figure(), Some(A));
    }

    #[test]
    fn a_figure_arriving_keeps_its_token() {
        // Given
        let mut placed = Placed::empty();

        // When
        placed.observe(figure(A, Some(TOKEN_A)));

        // Then
        assert_eq!(placed.token(), Some(TOKEN_A));
    }

    /// A figure whose token could not be read is still a figure. The box can
    /// play what the card already holds; only a download needs the token.
    #[test]
    fn a_figure_whose_token_was_not_read_is_still_announced() {
        // Given
        let mut placed = Placed::empty();

        // When
        let event = placed.observe(figure(A, None));

        // Then
        assert_eq!(event, Event::TagPresent(A));
        assert_eq!(placed.figure(), Some(A));
        assert_eq!(placed.token(), None);
    }

    /// A token that outlives its figure could authorise the *next* figure's
    /// download.
    #[test]
    fn a_figure_leaving_takes_its_token_with_it() {
        // Given
        let mut placed = Placed::empty();
        placed.observe(figure(A, Some(TOKEN_A)));

        // When
        let event = placed.observe(Seen::Nothing);

        // Then
        assert_eq!(event, Event::TagAbsent);
        assert_eq!(placed.figure(), None);
        assert_eq!(placed.token(), None);
    }

    /// The same rule during a swap: figure and token change together.
    #[test]
    fn a_replacing_figure_brings_its_own_token_and_not_the_last_one() {
        // Given
        let mut placed = Placed::empty();
        placed.observe(figure(A, Some(TOKEN_A)));

        // When
        let event = placed.observe(figure(B, Some(TOKEN_B)));

        // Then
        assert_eq!(event, Event::TagPresent(B));
        assert_eq!(placed.figure(), Some(B));
        assert_eq!(placed.token(), Some(TOKEN_B));
    }

    #[test]
    fn a_figure_replacing_one_that_had_a_token_does_not_inherit_it() {
        // Given
        let mut placed = Placed::empty();
        placed.observe(figure(A, Some(TOKEN_A)));

        // When
        placed.observe(figure(B, None));

        // Then
        assert_eq!(placed.token(), None, "B has no token of its own");
    }

    #[test]
    fn an_answer_naming_the_figure_on_the_plate_is_about_it() {
        // Given
        let mut placed = Placed::empty();
        placed.observe(figure(A, Some(TOKEN_A)));

        // When
        let answering = placed.answering(A.ruid());

        // Then
        assert_eq!(answering, Answering::TheFigure(A));
    }

    /// A console `get` that finishes while another figure is on the plate
    /// must not be reported for that figure.
    #[test]
    fn an_answer_naming_a_different_figure_is_not_about_the_one_on_the_plate() {
        // Given
        let mut placed = Placed::empty();
        placed.observe(figure(A, Some(TOKEN_A)));

        // When
        let answering = placed.answering(B.ruid());

        // Then
        assert_eq!(answering, Answering::AnotherFigure);
    }

    #[test]
    fn an_answer_arriving_at_an_empty_plate_is_about_nothing() {
        // Given
        let placed = Placed::empty();

        // When
        let answering = placed.answering(A.ruid());

        // Then
        assert_eq!(answering, Answering::NoFigure);
    }
}

#[cfg(test)]
mod settle_fetch_outcome_tests {
    use super::*;
    use teddiebox_download::Outcome;

    const A: TagUid = TagUid([1, 2, 3, 4, 5, 6, 7, 8]);

    /// The outcome reaches the reducer as the event for the figure it was
    /// fetched for.
    #[test]
    fn each_outcome_for_the_figure_on_the_plate_becomes_its_event() {
        // Given
        let outcomes = [
            Outcome::Completed,
            Outcome::Unreachable,
            Outcome::NoContent,
            Outcome::Refused,
        ];

        // When
        let settled = outcomes.map(|outcome| {
            (
                outcome,
                settle_fetch_outcome(Answering::TheFigure(A), outcome),
            )
        });

        // Then
        assert_eq!(
            settled,
            [
                (
                    Outcome::Completed,
                    Settlement::ForTheFigure(Event::ContentReady(A))
                ),
                (
                    Outcome::Unreachable,
                    Settlement::ForTheFigure(Event::ContentMissing(A, Unavailable::Unreachable))
                ),
                (
                    Outcome::NoContent,
                    Settlement::ForTheFigure(Event::ContentMissing(A, Unavailable::NoContent))
                ),
                (
                    Outcome::Refused,
                    Settlement::ForTheFigure(Event::ContentMissing(A, Unavailable::Refused))
                ),
            ]
        );
    }

    /// A console `get` that finished while another figure replaced the one it
    /// was fetching for must not be reported for that figure.
    #[test]
    fn an_outcome_for_another_figure_is_not_settled_against_the_one_on_the_plate() {
        // Given
        let answering = Answering::AnotherFigure;

        // When
        let settled = settle_fetch_outcome(answering, Outcome::Completed);

        // Then
        assert_eq!(settled, Settlement::ForAnotherFigure);
    }

    #[test]
    fn an_outcome_with_nobody_waiting_settles_nothing() {
        // Given
        let answering = Answering::NoFigure;

        // When
        let settled = settle_fetch_outcome(answering, Outcome::Completed);

        // Then
        assert_eq!(settled, Settlement::NothingWaiting);
    }
}

#[cfg(test)]
mod resolve_tests {
    use super::*;

    const A: TagUid = TagUid([1, 2, 3, 4, 5, 6, 7, 8]);

    #[test]
    fn resolving_the_figure_on_the_plate_calls_settle_with_it() {
        // Given
        let answering = Answering::TheFigure(A);

        // When
        let settled = answering.resolve(Event::ContentReady);

        // Then
        assert_eq!(settled, Settlement::ForTheFigure(Event::ContentReady(A)));
    }

    #[test]
    fn resolving_another_figure_never_calls_settle() {
        // Given
        let answering = Answering::AnotherFigure;

        // When
        let settled = answering.resolve(|_| panic!("must not be called"));

        // Then
        assert_eq!(settled, Settlement::<Event>::ForAnotherFigure);
    }

    #[test]
    fn resolving_with_nobody_waiting_never_calls_settle() {
        // Given
        let answering = Answering::NoFigure;

        // When
        let settled = answering.resolve(|_| panic!("must not be called"));

        // Then
        assert_eq!(settled, Settlement::<Event>::NothingWaiting);
    }
}
