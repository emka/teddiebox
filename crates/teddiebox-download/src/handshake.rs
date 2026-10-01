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
        // Given
        let mut h = Handshake::new();

        // When
        let step = h.requested(0);

        // Then
        assert_eq!(step, Step::AskCard);
        assert!(h.is_asking());
    }

    #[test]
    fn an_answer_that_names_cached_bytes_resumes_after_them() {
        // Given
        let mut h = Handshake::new();
        h.requested(0);

        // When
        let step = h.card_answered(CardSays::Holds(27_841_285));

        // Then
        assert_eq!(step, Step::Fetch { from: 27_841_285 });
        assert!(!h.is_asking());
    }

    #[test]
    fn an_answer_of_nothing_cached_fetches_from_the_beginning() {
        // Given
        let mut h = Handshake::new();
        h.requested(0);

        // When
        let step = h.card_answered(CardSays::Nothing);

        // Then
        assert_eq!(step, Step::Fetch { from: 0 });
    }

    #[test]
    fn a_file_already_whole_plays_instead_of_fetching() {
        // Given
        let mut h = Handshake::new();
        h.requested(0);

        // When
        let step = h.card_answered(CardSays::HoldsAll);

        // Then
        assert_eq!(step, Step::Play);
    }

    /// The first ask can be missed while a prompt is playing, so it is
    /// repeated.
    #[test]
    fn a_silent_card_is_asked_again() {
        // Given
        let mut h = Handshake::new();
        h.requested(0);

        // When
        let just_before = h.polled(u64::from(RETRY_MS) - 1);
        let at_the_retry = h.polled(u64::from(RETRY_MS));

        // Then
        assert_eq!(just_before, Step::Wait);
        assert_eq!(at_the_retry, Step::AskCard);
    }

    /// If the card is busy playing when a figure is placed, the download must
    /// not start from zero and truncate a partial file.
    #[test]
    fn a_card_that_never_answers_never_turns_into_a_fetch_from_zero() {
        // Given
        let mut h = Handshake::new();
        h.requested(0);

        // When
        let step = poll_until_it_gives_up(&mut h);

        // Then
        assert_eq!(step, Step::GiveUp);
    }

    /// Polled in 100 ms steps, like the network loop. (A single jump to the
    /// deadline would only trigger a re-ask.)
    #[test]
    fn the_conversation_is_abandoned_at_the_deadline() {
        // Given
        let mut h = Handshake::new();
        h.requested(0);

        // When
        let gave_up_early = (1..(DEADLINE_MS / 100))
            .map(|tick| h.polled(u64::from(tick) * 100))
            .any(|step| step == Step::GiveUp);
        let at_the_deadline = h.polled(u64::from(DEADLINE_MS));

        // Then
        assert!(!gave_up_early, "gave up early");
        assert_eq!(at_the_deadline, Step::GiveUp);
        assert!(!h.is_asking());
    }

    /// An answer just before the deadline is still in time.
    #[test]
    fn an_answer_just_before_the_deadline_still_fetches() {
        // Given
        let mut h = Handshake::new();
        h.requested(0);
        h.polled(u64::from(DEADLINE_MS) - 1);

        // When
        let step = h.card_answered(CardSays::Holds(42));

        // Then
        assert_eq!(step, Step::Fetch { from: 42 });
    }

    /// An answer can arrive after the handshake gave up. It must not start a
    /// download for a figure that may have been lifted.
    #[test]
    fn an_answer_after_giving_up_is_ignored() {
        // Given
        let mut h = Handshake::new();
        h.requested(0);
        poll_until_it_gives_up(&mut h);

        // When
        let step = h.card_answered(CardSays::Holds(42));

        // Then
        assert_eq!(step, Step::Wait);
    }

    #[test]
    fn polls_with_nothing_asked_do_nothing() {
        // Given
        let mut h = Handshake::new();

        // When
        let step = h.polled(u64::from(DEADLINE_MS) * 2);

        // Then
        assert_eq!(step, Step::Wait);
    }

    /// The deadline is fifteen seconds of real time, however slowly the
    /// caller polls.
    ///
    /// While a story plays, the media task blocks the executor and the
    /// network task's 10 ms timer only fires every ~106 ms (measured on the
    /// box).
    #[test]
    fn the_deadline_is_wall_clock_however_slowly_the_caller_polls() {
        // Given: a 10 ms timer that fires after 106 ms, as measured on the box
        const GAP_MS: u64 = 106;
        let mut h = Handshake::new();
        h.requested(0);

        // When
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

        // Then
        assert!(now >= u64::from(DEADLINE_MS), "gave up early, at {now} ms");
    }
}
