//! The exchange between the network task and the task that owns the SD card,
//! as a state machine with no I/O.
//!
//! Before downloading, the network task must know how much of the file is
//! already on the card; downloading it all again could take fifteen minutes.
//! Only the media task may use the card, and it is busy while a story or a
//! prompt plays, which is often when a figure is placed.
//!
//! Keeping this logic here means the case where the card does not answer can
//! be tested on the host.

/// How long the card gets before the question is put again.
///
/// Short, because asking again is cheap: it sets a flag that the media task
/// checks. The first ask can be missed while a prompt is playing.
pub const RETRY_MS: u32 = 500;

/// How long the whole conversation gets before it is abandoned.
///
/// The wait is for the media task to finish a prompt, which takes seconds.
/// Fifteen seconds is enough for that, but short enough that a figure is not
/// left silent for a minute. The LED shows the download colour meanwhile.
pub const DEADLINE_MS: u32 = 15_000;

/// What the card knows about a file that is partly, or wholly, there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardSays {
    /// Nothing usable is cached; ask for the file from the beginning.
    Nothing,
    /// This many bytes are cached and valid; resume after them.
    Holds(u32),
    /// The whole file is there. There is nothing to fetch.
    HoldsAll,
}

impl CardSays {
    /// What [`CardSays::as_offset`] returns for "there is nothing to ask the
    /// server for".
    ///
    /// A special value rather than a separate flag: `u32::MAX` can never be a
    /// real offset.
    pub const NOTHING_TO_FETCH: u32 = u32::MAX;

    /// The answer as one number, for a transport that can carry only one.
    ///
    /// The two tasks pass the answer through an atomic.
    pub const fn as_offset(self) -> u32 {
        match self {
            CardSays::Nothing => 0,
            CardSays::Holds(from) => from,
            CardSays::HoldsAll => Self::NOTHING_TO_FETCH,
        }
    }

    /// The number read back.
    ///
    /// `Nothing` and `Holds(0)` are both `0`, because both mean "download from
    /// the beginning" and [`Handshake::card_answered`] treats them the same.
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
    /// The card never answered. Report it, instead of waiting forever.
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
    /// Asks again every [`RETRY_MS`], and gives up at [`DEADLINE_MS`]. **Never
    /// falls back to downloading from zero**: that would truncate the partial
    /// file, losing everything already downloaded.
    ///
    /// Takes the current time, not the time since the last call. While a
    /// story plays, the network task's 10 ms timer only fires every ~106 ms,
    /// so adding up requested intervals would make the deadline ten times
    /// too long.
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
    /// An answer when nothing was asked is ignored: it belongs to an exchange
    /// that already gave up.
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

    /// Moves the clock forward until the handshake gives up, with a limit so
    /// a handshake that never gives up fails the test instead of hanging it.
    /// Also fails on `Fetch { from: 0 }`, which would truncate the cache.
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

    /// The first ask can be missed while a prompt is playing, so it is
    /// repeated.
    #[test]
    fn a_silent_card_is_asked_again() {
        let mut h = Handshake::new();
        h.requested(0);
        assert_eq!(h.polled(u64::from(RETRY_MS) - 1), Step::Wait);
        assert_eq!(h.polled(u64::from(RETRY_MS)), Step::AskCard);
    }

    /// If the card is busy playing when a figure is placed, the download must
    /// not start from zero and truncate a partial file.
    #[test]
    fn a_card_that_never_answers_never_turns_into_a_fetch_from_zero() {
        let mut h = Handshake::new();
        h.requested(0);

        assert_eq!(poll_until_it_gives_up(&mut h), Step::GiveUp);
    }

    /// Polled in 100 ms steps, like the network loop. (A single jump to the
    /// deadline would only trigger a re-ask.)
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

    /// An answer just before the deadline is still in time.
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

    /// An answer can arrive after the handshake gave up. It must not start a
    /// download for a figure that may have been lifted.
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

    /// The deadline is fifteen seconds of real time, however slowly the
    /// caller polls.
    ///
    /// While a story plays, the media task blocks the executor and the
    /// network task's 10 ms timer only fires every ~106 ms (measured on the
    /// box).
    #[test]
    fn the_deadline_is_wall_clock_however_slowly_the_caller_polls() {
        let mut h = Handshake::new();
        h.requested(0);

        // As measured on the box: a 10 ms timer that fires after 106 ms.
        const GAP_MS: u64 = 106;

        let mut now = 0;
        loop {
            let step = h.polled(now);
            assert_ne!(step, Step::Fetch { from: 0 }, "truncated the cache");
            if step == Step::GiveUp {
                break;
            }
            now += GAP_MS;
            // Allow one gap of slack: giving up is only seen on the first
            // reading after the deadline.
            assert!(
                now <= u64::from(DEADLINE_MS) + GAP_MS,
                "still asking after {now} ms of wall clock"
            );
        }
        assert!(now >= u64::from(DEADLINE_MS), "gave up early, at {now} ms");
    }

    /// Whatever the card said must mean the same after passing through the
    /// atomic.
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

    /// Mixing these up would either resume after four gigabytes or play a
    /// partial file.
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
