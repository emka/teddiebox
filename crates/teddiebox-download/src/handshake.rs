//! The conversation between the task that fetches and the task that owns the
//! card, as a state machine with no I/O in it.
//!
//! The net task cannot start a download until it knows how much of the file is
//! already there: asking for the whole thing when most of it is cached throws
//! away a quarter of an hour, and the card is the only thing that knows. But
//! only the media task may touch the card, and it is busy exactly when a story
//! or a prompt is playing — which is also when a figure is most likely to be
//! placed.
//!
//! So the ask is a conversation, and this is the whole of it. Keeping it here
//! rather than in the firmware's loops is what makes the interesting case — the
//! card not answering — a test rather than a bench session.

/// How long the card gets before the question is put again.
///
/// Short, because re-asking costs nothing: it sets a flag the media task reads
/// between requests. The point of asking again is that the first ask can land
/// while a prompt is playing and be missed entirely.
pub const RETRY_MS: u32 = 500;

/// How long the whole conversation gets before it is abandoned.
///
/// The thing being waited for is the media task finishing whatever is on the
/// speaker, and a prompt is seconds. Fifteen of them is generous for that and
/// still short enough that a figure is not left silent for a minute — the
/// indicator is on the server's colour throughout, so the wait is at least
/// explained while it lasts.
pub const DEADLINE_MS: u32 = 15_000;

/// What the card knows about a file that is partly, or wholly, there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardSays {
    /// Nothing usable is cached; ask for the file from the beginning.
    Nothing,
    /// This many bytes are cached and vouched for; resume after them.
    Holds(u32),
    /// The whole file is there. There is nothing to fetch.
    HoldsAll,
}

impl CardSays {
    /// What [`CardSays::as_offset`] returns for "there is nothing to ask the
    /// server for".
    ///
    /// A sentinel rather than a second flag beside the number, because it is
    /// one answer to one question — where does this download start — and
    /// `u32::MAX` is not a plausible offset in a file a card could hold.
    pub const NOTHING_TO_FETCH: u32 = u32::MAX;

    /// The answer as one number, for a transport that can carry only one.
    ///
    /// On the box this crosses between the task that owns the card and the
    /// task that fetches, which share nothing they can pass a value through
    /// but an atomic.
    pub const fn as_offset(self) -> u32 {
        match self {
            CardSays::Nothing => 0,
            CardSays::Holds(from) => from,
            CardSays::HoldsAll => Self::NOTHING_TO_FETCH,
        }
    }

    /// The number read back.
    ///
    /// `Nothing` and `Holds(0)` share the representation `0`, because they are
    /// the same instruction — fetch from the beginning — and
    /// [`Handshake::card_answered`] turns both into the same step. The round
    /// trip is therefore faithful in meaning rather than in variant.
    pub const fn from_offset(offset: u32) -> Self {
        match offset {
            Self::NOTHING_TO_FETCH => CardSays::HoldsAll,
            0 => CardSays::Nothing,
            from => CardSays::Holds(from),
        }
    }
}

/// What the caller should do about the conversation now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Nothing this pass.
    Wait,
    /// Ask the card how much is already there.
    AskCard,
    /// Open the connection, resuming after `from` bytes.
    Fetch { from: u32 },
    /// The card already holds the whole file. Whoever asked can play now.
    Play,
    /// The card never answered. Tell whoever asked, rather than leaving a
    /// figure waiting on something that is not coming.
    GiveUp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    Asking { started_ms: u64, asked_ms: u64 },
}

/// Tracks one download handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Handshake {
    state: State,
}

impl Default for Handshake {
    fn default() -> Self {
        Self::new()
    }
}

impl Handshake {
    pub const fn new() -> Self {
        Self { state: State::Idle }
    }

    /// A fetch has been asked for. Starts the conversation, at `now_ms`.
    pub fn requested(&mut self, now_ms: u64) -> Step {
        self.state = State::Asking {
            started_ms: now_ms,
            asked_ms: now_ms,
        };
        Step::AskCard
    }

    /// The clock has been read again with no answer yet.
    ///
    /// Re-asks every [`RETRY_MS`], and abandons the conversation at
    /// [`DEADLINE_MS`]. **Never falls back to fetching from zero**, which is
    /// what it used to do: a fresh download truncates the partial file it lands
    /// on, so a card that was merely slow to answer loses everything it had
    /// cached and starts the quarter of an hour again.
    ///
    /// Takes the reading rather than the interval since the last one, because
    /// the caller is a task that cannot know how long its own `await` took.
    /// The net task polls on a 10 ms timer and, while a story plays, gets it
    /// back every ~106 ms — so a caller reporting the interval it *asked* for
    /// stretched this deadline to 158.6 s on the box, and only ever under the
    /// load the deadline exists for. A clock cannot be got wrong that way.
    pub fn polled(&mut self, now_ms: u64) -> Step {
        let State::Asking {
            started_ms,
            asked_ms,
        } = &mut self.state
        else {
            return Step::Wait;
        };

        if now_ms.saturating_sub(*started_ms) >= u64::from(DEADLINE_MS) {
            self.state = State::Idle;
            return Step::GiveUp;
        }
        if now_ms.saturating_sub(*asked_ms) >= u64::from(RETRY_MS) {
            *asked_ms = now_ms;
            return Step::AskCard;
        }
        Step::Wait
    }

    /// The card has answered.
    ///
    /// An answer arriving when nothing was asked is ignored: it belongs to a
    /// conversation that has already been abandoned, and acting on it would
    /// start a download nobody is waiting for.
    pub fn card_answered(&mut self, says: CardSays) -> Step {
        if !matches!(self.state, State::Asking { .. }) {
            return Step::Wait;
        }
        self.state = State::Idle;
        match says {
            CardSays::Nothing => Step::Fetch { from: 0 },
            CardSays::Holds(from) => Step::Fetch { from },
            CardSays::HoldsAll => Step::Play,
        }
    }

    /// Whether a conversation is in progress.
    pub fn is_asking(&self) -> bool {
        matches!(self.state, State::Asking { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Polls a clock forward until the conversation is abandoned, and refuses
    /// to do so for ever.
    ///
    /// The bound is not decoration. Written as `while h.polled(now) !=
    /// GiveUp {}` this hung outright the moment the implementation stopped
    /// giving up — which is exactly the change it is here to catch, so the
    /// test that should have failed in a millisecond took a test run down
    /// with it instead. Anything that returns `Fetch { from: 0 }` on the way
    /// fails here too: that is the truncation this type exists to prevent.
    fn poll_until_it_gives_up(h: &mut Handshake) -> Step {
        let mut now = 0;
        for _ in 0..(DEADLINE_MS / 100 + 10) {
            now += 100;
            let step = h.polled(now);
            assert_ne!(step, Step::Fetch { from: 0 }, "truncated the cache");
            if step == Step::GiveUp {
                return step;
            }
        }
        panic!("never gave up");
    }

    #[test]
    fn a_request_asks_the_card_first() {
        let mut h = Handshake::new();
        assert_eq!(h.requested(0), Step::AskCard);
        assert!(h.is_asking());
    }

    #[test]
    fn an_answer_that_names_cached_bytes_resumes_after_them() {
        let mut h = Handshake::new();
        h.requested(0);
        assert_eq!(
            h.card_answered(CardSays::Holds(27_841_285)),
            Step::Fetch { from: 27_841_285 }
        );
        assert!(!h.is_asking());
    }

    #[test]
    fn an_answer_of_nothing_cached_fetches_from_the_beginning() {
        let mut h = Handshake::new();
        h.requested(0);
        assert_eq!(h.card_answered(CardSays::Nothing), Step::Fetch { from: 0 });
    }

    #[test]
    fn a_file_already_whole_plays_instead_of_fetching() {
        let mut h = Handshake::new();
        h.requested(0);
        assert_eq!(h.card_answered(CardSays::HoldsAll), Step::Play);
    }

    /// The first ask can land while a prompt is playing, and the media task
    /// only looks between requests — so it has to be put again.
    #[test]
    fn a_silent_card_is_asked_again() {
        let mut h = Handshake::new();
        h.requested(0);
        assert_eq!(h.polled(u64::from(RETRY_MS) - 1), Step::Wait);
        assert_eq!(h.polled(u64::from(RETRY_MS)), Step::AskCard);
    }

    /// The bug this whole type exists for. A story or a prompt on the speaker
    /// when a figure is placed used to hold the card past the five-second wait,
    /// and the fetch then went ahead with "nothing is cached" — truncating a
    /// part-downloaded file and starting the quarter of an hour again.
    #[test]
    fn a_card_that_never_answers_never_turns_into_a_fetch_from_zero() {
        let mut h = Handshake::new();
        h.requested(0);

        assert_eq!(poll_until_it_gives_up(&mut h), Step::GiveUp);
    }

    /// Polled the way the net loop polls it, 100 ms at a time, because the
    /// gap between two readings decides how often the question is put again —
    /// a single reading the length of the whole deadline is a re-ask, not a
    /// wait.
    #[test]
    fn the_conversation_is_abandoned_at_the_deadline() {
        let mut h = Handshake::new();
        h.requested(0);
        for tick in 1..(DEADLINE_MS / 100) {
            assert_ne!(
                h.polled(u64::from(tick) * 100),
                Step::GiveUp,
                "gave up early"
            );
        }
        assert_eq!(h.polled(u64::from(DEADLINE_MS)), Step::GiveUp);
        assert!(!h.is_asking());
    }

    /// A card that answers on the last tick is still in time. The figure gets
    /// its story rather than the prompt that says it could not be had.
    #[test]
    fn an_answer_just_before_the_deadline_still_fetches() {
        let mut h = Handshake::new();
        h.requested(0);
        h.polled(u64::from(DEADLINE_MS) - 1);
        assert_eq!(
            h.card_answered(CardSays::Holds(42)),
            Step::Fetch { from: 42 }
        );
    }

    /// The media task is polling a flag, so an answer can arrive after the
    /// conversation was abandoned. Acting on it would start a download nobody
    /// is waiting for, against a figure that may have been lifted.
    #[test]
    fn an_answer_after_giving_up_is_ignored() {
        let mut h = Handshake::new();
        h.requested(0);
        assert_eq!(poll_until_it_gives_up(&mut h), Step::GiveUp);
        assert_eq!(h.card_answered(CardSays::Holds(42)), Step::Wait);
    }

    #[test]
    fn polls_with_nothing_asked_do_nothing() {
        let mut h = Handshake::new();
        assert_eq!(h.polled(u64::from(DEADLINE_MS) * 2), Step::Wait);
    }

    /// The deadline is fifteen seconds of wall clock, however slowly the
    /// caller gets round to asking.
    ///
    /// Measured on the box on 2026-09-23, and the reason this takes a clock
    /// rather than a duration. The net task polls on a 10 ms timer, but while
    /// a story plays the media task blocks the executor and the timer only
    /// comes back every ~106 ms. Told "10 ms" each time, the conversation
    /// reached its deadline after **158.6 seconds** — so the load the deadline
    /// exists for is precisely the load that inflates it, by ten times, with
    /// the box still printing "15 s".
    #[test]
    fn the_deadline_is_wall_clock_however_slowly_the_caller_polls() {
        let mut h = Handshake::new();
        h.requested(0);

        // What the box actually measured: a poll that asked for 10 ms and got
        // 106.
        const GAP_MS: u64 = 106;

        let mut now = 0;
        loop {
            let step = h.polled(now);
            assert_ne!(step, Step::Fetch { from: 0 }, "truncated the cache");
            if step == Step::GiveUp {
                break;
            }
            now += GAP_MS;
            // One gap of slack, and no more: giving up can only be *seen* on
            // the reading after the clock passes the deadline. The bug this
            // test is here for overshot by ten times that.
            assert!(
                now <= u64::from(DEADLINE_MS) + GAP_MS,
                "still asking after {now} ms of wall clock"
            );
        }
        assert!(now >= u64::from(DEADLINE_MS), "gave up early, at {now} ms");
    }

    /// The transport carries meaning, not variants: whatever the card said
    /// must come back as the same instruction on the other side.
    #[test]
    fn every_answer_survives_the_trip_between_the_two_tasks() {
        for says in [
            CardSays::Nothing,
            CardSays::Holds(1),
            CardSays::Holds(4_194_304),
            CardSays::HoldsAll,
        ] {
            let there_and_back = CardSays::from_offset(says.as_offset());
            let mut direct = Handshake::new();
            direct.requested(0);
            let mut round_tripped = Handshake::new();
            round_tripped.requested(0);

            assert_eq!(
                direct.card_answered(says),
                round_tripped.card_answered(there_and_back),
                "{says:?} arrived as {there_and_back:?} and meant something else",
            );
        }
    }

    /// The one that would be silent if it were wrong. A sentinel read as an
    /// offset asks the server to resume after four gigabytes; an offset read
    /// as the sentinel plays a file that is only partly there.
    #[test]
    fn a_full_card_is_not_confusable_with_an_offset() {
        assert_eq!(
            CardSays::from_offset(CardSays::HoldsAll.as_offset()),
            CardSays::HoldsAll
        );
        assert_ne!(
            CardSays::Holds(1).as_offset(),
            CardSays::HoldsAll.as_offset()
        );
        assert_ne!(
            CardSays::Nothing.as_offset(),
            CardSays::HoldsAll.as_offset()
        );
    }
}
