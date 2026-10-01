//! Whether a story already on the card is still the story the server has.
//!
//! The check compares the file's length, the one thing teddyCloud reliably
//! reports. This is weaker than an ETag: a new version with exactly the same
//! length looks current. Story audio rarely changes, so that is acceptable.
//!
//! teddyCloud sends no ETag for content it already has. Whether it passes one
//! on for content it fetches from the Tonies cloud has not been measured. The
//! length check works either way.
//!
//! Most important: when there is no answer (server unreachable, figure
//! unknown to the cloud, no length in the reply), the file on the card is
//! kept. Revalidation must never make a working box worse than being offline.

use crate::Sidecar;
use teddiebox_cloud::Probed;

/// How many figures are remembered as asked-about since boot.
///
/// Eight is plenty for one play session. Going over only costs one extra
/// check for the ninth figure.
pub const REMEMBERED: usize = 8;

/// Whether the cached file is out of date and must be fetched again.
///
/// Only a length the server actually *gave*, and that differs from the
/// sidecar's, makes a file stale. Anything else returns `false`, and the
/// cached copy plays.
pub fn is_stale(cached: &Sidecar, probed: Probed) -> bool {
    match probed {
        Probed::Length(total) => total != cached.length,
        // The server has no story for this figure. Keep the card's copy; it
        // may be the only one left.
        Probed::NoContent => false,
        // The server answered without a length, so nothing is contradicted.
        Probed::Unstated => false,
    }
}

/// Which figures have already been asked about since the box booted.
///
/// Asking uses Wi-Fi, which uses the most battery and delays the start of a
/// story by seconds. The box switches off after five idle minutes, so one
/// boot is roughly one play session, and each figure is checked once per
/// session. This also needs no clock, which the box does not have.
///
/// A figure is remembered when its answer *arrives*, whatever the answer. A
/// server that was just unreachable will probably stay unreachable for the
/// rest of the session.
#[derive(Debug, Default)]
pub struct Asked {
    /// Ruids, oldest first. The oldest is evicted when a ninth arrives.
    ruids: heapless::Deque<u64, REMEMBERED>,
}

impl Asked {
    pub const fn new() -> Self {
        Self {
            ruids: heapless::Deque::new(),
        }
    }

    pub fn contains(&self, ruid: u64) -> bool {
        self.ruids.iter().any(|&seen| seen == ruid)
    }

    /// Records that this figure has been asked about. Recording it twice does
    /// not push anything else out.
    pub fn remember(&mut self, ruid: u64) {
        if self.contains(ruid) {
            return;
        }
        if self.ruids.is_full() {
            let _ = self.ruids.pop_front();
        }
        let _ = self.ruids.push_back(ruid);
    }

    /// Forgets everything, so the next placement asks again.
    ///
    /// For the console: after changing a file on the server, this makes the
    /// box ask again without a power cycle.
    pub fn forget_all(&mut self) {
        self.ruids.clear();
    }
}

/// How long a figure waits on the server before the box plays what it has.
///
/// Asking needs a Wi-Fi connection. Out of range, the box still tries, and
/// the DHCP wait alone is 20 s. A child whose story is already on the card
/// should not wait that long.
///
/// A successful check takes a few seconds, well within this limit.
pub const PATIENCE_MS: u64 = 10_000;

/// What the server said about a figure's length, reduced to what the box can
/// act on.
///
/// `Nothing` covers every answer that is not a length: no story, no length,
/// unreachable, Wi-Fi failed, or the time ran out. The box does the same for
/// all of them: plays the card. The console logs which one it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    Length(u32),
    Nothing,
}

/// A question that is over, and what it came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settled {
    pub ruid: u64,
    pub answer: Answer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    Asking { ruid: u64, asked_ms: u64 },
}

/// The exchange between the task that owns the card and the task that asks
/// the server, one question at a time.
///
/// **Each question is settled at most once.** An answer that arrives after
/// the time ran out, or after the question was already settled, returns
/// `None`, so nothing acts on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Revalidation {
    state: State,
}

impl Default for Revalidation {
    fn default() -> Self {
        Self::new()
    }
}

impl Revalidation {
    pub const fn new() -> Self {
        Self { state: State::Idle }
    }

    /// A question was asked about this figure at `now_ms`. Replaces any open
    /// question: the reducer only waits on one figure at a time.
    pub fn asked(&mut self, ruid: u64, now_ms: u64) {
        self.state = State::Asking {
            ruid,
            asked_ms: now_ms,
        };
    }

    /// The figure was lifted. Ends the question without settling it; a later
    /// answer is ignored, even if the same figure is back, because it will be
    /// asked about again.
    pub fn withdrawn(&mut self) {
        self.state = State::Idle;
    }

    /// The clock has been read with no answer yet.
    ///
    /// Takes the current time rather than an interval, like
    /// [`crate::Handshake::polled`].
    pub fn polled(&mut self, now_ms: u64) -> Option<Settled> {
        let State::Asking { ruid, asked_ms } = self.state else {
            return None;
        };
        if now_ms.saturating_sub(asked_ms) < PATIENCE_MS {
            return None;
        }
        self.state = State::Idle;
        Some(Settled {
            ruid,
            answer: Answer::Nothing,
        })
    }

    /// An answer has arrived. Settles the question only if it is about the
    /// figure still being asked about.
    pub fn answered(&mut self, ruid: u64, answer: Answer) -> Option<Settled> {
        match self.state {
            State::Asking { ruid: open, .. } if open == ruid => {
                self.state = State::Idle;
                Some(Settled { ruid, answer })
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use teddiebox_cloud::ETag;

    fn sidecar(length: u32) -> Sidecar {
        Sidecar { length, etag: None }
    }

    /// Real numbers: `1d2e3f50500304e0` is 37,912,939 bytes on the server and
    /// on the card.
    #[test]
    fn a_length_that_matches_is_current() {
        // Given
        let on_card = sidecar(37_912_939);

        // When
        let stale = is_stale(&on_card, Probed::Length(37_912_939));

        // Then
        assert!(!stale);
    }

    #[test]
    fn a_length_that_differs_is_stale_whichever_way_it_moved() {
        // Given
        let on_card = sidecar(37_912_939);

        // When
        let longer = is_stale(&on_card, Probed::Length(37_912_940));
        let shorter = is_stale(&on_card, Probed::Length(1_024));

        // Then
        assert!(longer);
        assert!(shorter);
    }

    /// The most important case: a figure the server has no story for keeps
    /// the story on the card.
    #[test]
    fn a_server_with_no_story_never_discards_the_one_on_the_card() {
        // Given
        let on_card = sidecar(37_912_939);

        // When
        let stale = is_stale(&on_card, Probed::NoContent);

        // Then
        assert!(!stale);
    }

    #[test]
    fn an_answer_without_a_length_changes_nothing() {
        // Given
        let on_card = sidecar(37_912_939);

        // When
        let stale = is_stale(&on_card, Probed::Unstated);

        // Then
        assert!(!stale);
    }

    /// An etag in the sidecar is ignored; only the length counts.
    #[test]
    fn the_recorded_etag_does_not_enter_into_it() {
        // Given
        let with_etag = Sidecar {
            length: 4_096,
            etag: Some(ETag::try_from("\"v1\"").unwrap()),
        };

        // When
        let longer = is_stale(&with_etag, Probed::Length(8_192));
        let same = is_stale(&with_etag, Probed::Length(4_096));

        // Then
        assert!(longer);
        assert!(!same);
    }

    #[test]
    fn a_figure_asked_about_once_is_not_asked_again() {
        // Given
        let mut asked = Asked::new();
        let before = asked.contains(0x1D2E_3F50_5003_04E0);

        // When
        asked.remember(0x1D2E_3F50_5003_04E0);

        // Then
        assert!(!before);
        assert!(asked.contains(0x1D2E_3F50_5003_04E0));
    }

    #[test]
    fn remembering_the_same_figure_twice_costs_no_room() {
        // Given
        let mut asked = Asked::new();

        // When
        for _ in 0..REMEMBERED * 2 {
            asked.remember(1);
        }
        asked.remember(2);

        // Then
        assert!(asked.contains(1), "one crowding itself out is the bug");
        assert!(asked.contains(2));
    }

    /// The ninth figure pushes out the oldest, not the newest (which is
    /// probably the one on the plate).
    #[test]
    fn a_ninth_figure_pushes_out_the_oldest() {
        // Given
        let mut asked = Asked::new();
        for ruid in 0..REMEMBERED as u64 {
            asked.remember(ruid);
        }

        // When
        asked.remember(99);

        // Then
        assert!(!asked.contains(0));
        assert!(asked.contains(1));
        assert!(asked.contains(99));
    }

    /// After replacing a file on the server, the box can be made to ask
    /// again without a power cycle.
    #[test]
    fn forgetting_makes_every_figure_ask_again() {
        // Given
        let mut asked = Asked::new();
        asked.remember(7);

        // When
        asked.forget_all();

        // Then
        assert!(!asked.contains(7));
    }

    const FIGURE: u64 = 0x1D2E_3F50_5003_04E0;
    const OTHER: u64 = 0x1E2F_4051_5003_04E0;

    #[test]
    fn an_answer_to_the_open_question_settles_it() {
        // Given
        let mut r = Revalidation::new();
        r.asked(FIGURE, 0);

        // When
        let settled = r.answered(FIGURE, Answer::Length(37_912_939));

        // Then
        assert_eq!(
            settled,
            Some(Settled {
                ruid: FIGURE,
                answer: Answer::Length(37_912_939)
            })
        );
    }

    #[test]
    fn a_question_nobody_answers_settles_as_nothing_when_patience_runs_out() {
        // Given
        let mut r = Revalidation::new();
        r.asked(FIGURE, 0);

        // When
        let just_before = r.polled(9_999);
        let at_the_limit = r.polled(10_000);

        // Then
        assert_eq!(just_before, None);
        assert_eq!(
            at_the_limit,
            Some(Settled {
                ruid: FIGURE,
                answer: Answer::Nothing
            })
        );
    }

    /// A late answer must be ignored. Otherwise it could mark a playing
    /// card copy stale, and the next download would truncate it.
    #[test]
    fn an_answer_after_patience_ran_out_is_ignored() {
        // Given
        let mut r = Revalidation::new();
        r.asked(FIGURE, 0);
        r.polled(10_000);

        // When
        let settled = r.answered(FIGURE, Answer::Length(1_024));

        // Then
        assert_eq!(settled, None);
    }

    /// A figure lifted and put back during a question causes a second check.
    /// The first answer settles the question; the second is ignored.
    #[test]
    fn an_answer_after_the_question_settled_is_ignored() {
        // Given
        let mut r = Revalidation::new();
        r.asked(FIGURE, 0);
        r.answered(FIGURE, Answer::Length(37_912_939));

        // When
        let settled = r.answered(FIGURE, Answer::Length(1_024));

        // Then
        assert_eq!(settled, None);
    }

    #[test]
    fn an_answer_about_another_figure_does_not_settle_the_question() {
        // Given
        let mut r = Revalidation::new();
        r.asked(FIGURE, 0);

        // When
        let about_other = r.answered(OTHER, Answer::Nothing);
        let about_figure = r.answered(FIGURE, Answer::Nothing);

        // Then
        assert_eq!(about_other, None);
        assert_eq!(
            about_figure,
            Some(Settled {
                ruid: FIGURE,
                answer: Answer::Nothing
            })
        );
    }

    #[test]
    fn a_new_question_replaces_the_open_one() {
        // Given
        let mut r = Revalidation::new();
        r.asked(FIGURE, 0);
        r.asked(OTHER, 100);

        // When
        let settled = r.answered(FIGURE, Answer::Length(1_024));

        // Then
        assert_eq!(settled, None);
    }

    #[test]
    fn patience_is_counted_from_the_latest_question() {
        // Given
        let mut r = Revalidation::new();
        r.asked(FIGURE, 0);
        r.asked(OTHER, 5_000);

        // When
        let at_first_deadline = r.polled(10_000);
        let at_second_deadline = r.polled(15_000);

        // Then
        assert_eq!(at_first_deadline, None);
        assert_eq!(
            at_second_deadline,
            Some(Settled {
                ruid: OTHER,
                answer: Answer::Nothing
            })
        );
    }

    /// A lift ends the question, so an answer after it is ignored, even if
    /// the figure is put back before the answer arrives.
    #[test]
    fn an_answer_after_the_figure_was_lifted_is_ignored() {
        // Given
        let mut r = Revalidation::new();
        r.asked(FIGURE, 0);
        r.withdrawn();

        // When
        let settled = r.answered(FIGURE, Answer::Length(1_024));

        // Then
        assert_eq!(settled, None);
    }

    /// Nobody waits for a lifted figure, so it does not time out.
    #[test]
    fn a_lifted_figure_never_runs_out_of_patience() {
        // Given
        let mut r = Revalidation::new();
        r.asked(FIGURE, 0);
        r.withdrawn();

        // When
        let settled = r.polled(10_000);

        // Then
        assert_eq!(settled, None);
    }

    #[test]
    fn a_clock_reading_before_the_ask_does_not_expire_it() {
        // Given
        let mut r = Revalidation::new();
        r.asked(FIGURE, 20_000);

        // When
        let settled = r.polled(0);

        // Then
        assert_eq!(settled, None);
    }

    #[test]
    fn polls_with_nothing_asked_do_nothing() {
        // Given
        let mut r = Revalidation::new();

        // When
        let early = r.polled(0);
        let late = r.polled(1_000_000);

        // Then
        assert_eq!(early, None);
        assert_eq!(late, None);
    }

    /// The media loop polls this, and under a story its 10 ms timer fires
    /// only every ~106 ms (measured). The limit must be real time regardless.
    #[test]
    fn the_patience_is_wall_clock_however_slowly_the_caller_polls() {
        // Given
        const GAP_MS: u64 = 106;
        let mut r = Revalidation::new();
        r.asked(FIGURE, 0);

        // When
        let mut now = 0;
        while r.polled(now).is_none() {
            now += GAP_MS;
            assert!(now <= 10_000 + GAP_MS, "still waiting at {now} ms");
        }

        // Then
        assert!(now >= 10_000, "gave up early, at {now} ms");
    }
}
