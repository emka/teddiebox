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
    Asking { waited: u32, since_ask: u32 },
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

    /// A fetch has been asked for. Starts the conversation.
    pub fn requested(&mut self) -> Step {
        self.state = State::Asking {
            waited: 0,
            since_ask: 0,
        };
        Step::AskCard
    }

    /// Time has passed with no answer.
    ///
    /// Re-asks every [`RETRY_MS`], and abandons the conversation at
    /// [`DEADLINE_MS`]. **Never falls back to fetching from zero**, which is
    /// what it used to do: a fresh download truncates the partial file it lands
    /// on, so a card that was merely slow to answer loses everything it had
    /// cached and starts the quarter of an hour again.
    pub fn ticked(&mut self, ms: u32) -> Step {
        let State::Asking { waited, since_ask } = &mut self.state else {
            return Step::Wait;
        };
        *waited = waited.saturating_add(ms);
        *since_ask = since_ask.saturating_add(ms);

        if *waited >= DEADLINE_MS {
            self.state = State::Idle;
            return Step::GiveUp;
        }
        if *since_ask >= RETRY_MS {
            *since_ask = 0;
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

    /// Ticks until the conversation is abandoned, and refuses to do so for
    /// ever.
    ///
    /// The bound is not decoration. Written as `while h.ticked(100) !=
    /// GiveUp {}` this hung outright the moment the implementation stopped
    /// giving up — which is exactly the change it is here to catch, so the
    /// test that should have failed in a millisecond took a test run down
    /// with it instead. Anything that returns `Fetch { from: 0 }` on the way
    /// fails here too: that is the truncation this type exists to prevent.
    fn tick_until_it_gives_up(h: &mut Handshake) -> Step {
        for _ in 0..(DEADLINE_MS / 100 + 10) {
            let step = h.ticked(100);
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
        assert_eq!(h.requested(), Step::AskCard);
        assert!(h.is_asking());
    }

    #[test]
    fn an_answer_that_names_cached_bytes_resumes_after_them() {
        let mut h = Handshake::new();
        h.requested();
        assert_eq!(
            h.card_answered(CardSays::Holds(27_841_285)),
            Step::Fetch { from: 27_841_285 }
        );
        assert!(!h.is_asking());
    }

    #[test]
    fn an_answer_of_nothing_cached_fetches_from_the_beginning() {
        let mut h = Handshake::new();
        h.requested();
        assert_eq!(h.card_answered(CardSays::Nothing), Step::Fetch { from: 0 });
    }

    #[test]
    fn a_file_already_whole_plays_instead_of_fetching() {
        let mut h = Handshake::new();
        h.requested();
        assert_eq!(h.card_answered(CardSays::HoldsAll), Step::Play);
    }

    /// The first ask can land while a prompt is playing, and the media task
    /// only looks between requests — so it has to be put again.
    #[test]
    fn a_silent_card_is_asked_again() {
        let mut h = Handshake::new();
        h.requested();
        assert_eq!(h.ticked(RETRY_MS - 1), Step::Wait);
        assert_eq!(h.ticked(1), Step::AskCard);
    }

    /// The bug this whole type exists for. A story or a prompt on the speaker
    /// when a figure is placed used to hold the card past the five-second wait,
    /// and the fetch then went ahead with "nothing is cached" — truncating a
    /// part-downloaded file and starting the quarter of an hour again.
    #[test]
    fn a_card_that_never_answers_never_turns_into_a_fetch_from_zero() {
        let mut h = Handshake::new();
        h.requested();

        assert_eq!(tick_until_it_gives_up(&mut h), Step::GiveUp);
    }

    /// Ticked the way the net loop ticks it, 100 ms at a time, because the
    /// size of a tick decides how often the question is put again — a single
    /// tick the length of the whole deadline is a re-ask, not a wait.
    #[test]
    fn the_conversation_is_abandoned_at_the_deadline() {
        let mut h = Handshake::new();
        h.requested();
        for _ in 0..(DEADLINE_MS / 100 - 1) {
            assert_ne!(h.ticked(100), Step::GiveUp, "gave up early");
        }
        assert_eq!(h.ticked(100), Step::GiveUp);
        assert!(!h.is_asking());
    }

    /// A card that answers on the last tick is still in time. The figure gets
    /// its story rather than the prompt that says it could not be had.
    #[test]
    fn an_answer_just_before_the_deadline_still_fetches() {
        let mut h = Handshake::new();
        h.requested();
        for _ in 0..(DEADLINE_MS / 100 - 1) {
            h.ticked(100);
        }
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
        h.requested();
        assert_eq!(tick_until_it_gives_up(&mut h), Step::GiveUp);
        assert_eq!(h.card_answered(CardSays::Holds(42)), Step::Wait);
    }

    #[test]
    fn ticks_with_nothing_asked_do_nothing() {
        let mut h = Handshake::new();
        assert_eq!(h.ticked(DEADLINE_MS * 2), Step::Wait);
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
            direct.requested();
            let mut round_tripped = Handshake::new();
            round_tripped.requested();

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
