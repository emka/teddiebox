//! Whether a story already on the card is still the story the server has.
//!
//! No ETag has ever been seen on a response to this box, so the question is
//! answered with the one number the server does state: how long the file is.
//! Weaker than an ETag and honest about it — a figure's audio does not change
//! silently, and a re-encode to exactly the same length reads as current.
//!
//! **"No ETag, ever" is contested and is deliberately not claimed here.** The
//! TLS design says absent on every route, local or proxied; a later review
//! says a *forwarded* response carries both `ETag` and `Last-Modified`, and
//! the client does emit `If-None-Match` and parse a `304`. Nothing measured on
//! 2026-09-13 settles it: those probes were all for content the server already
//! held. The length rule needs no answer either way, which is why it is the
//! rule — but do not repeat either claim as fact without measuring a figure
//! teddyCloud has to fetch upstream.
//!
//! The rule that matters more than the comparison is what happens when there
//! is no answer. A server that cannot be reached, a figure the cloud has never
//! heard of, a reply with no length in it: none of those is evidence against
//! a file that is sitting on the card and plays. Revalidation may never make a
//! working box worse than it was offline.

use crate::Sidecar;
use teddiebox_cloud::Probed;

/// How many figures are remembered as asked-about since boot.
///
/// A session is one child and the figures within reach of them. Eight is
/// generous for that, and the cost of overflowing is one extra probe on the
/// ninth figure — a few hundred bytes and a radio that was about to be raised
/// anyway.
pub const REMEMBERED: usize = 8;

/// Whether the cached file is out of date and must be fetched again.
///
/// Only a length the server actually *stated*, and which differs from what the
/// sidecar promised, makes a file stale. Everything else is `false`, which
/// means the cached copy plays.
pub fn is_stale(cached: &Sidecar, probed: Probed) -> bool {
    match probed {
        Probed::Length(total) => total != cached.length,
        // The server has no story for this figure at all. That is an answer
        // about the *server*, and a reason to keep what is on the card rather
        // than to throw it away — the card copy may be the last one in
        // existence.
        Probed::NoContent => false,
        // It answered, and said nothing about length. Nothing was contradicted.
        Probed::Unstated => false,
    }
}

/// Which figures have already been asked about since the box booted.
///
/// Asking costs the radio, which is the largest consumer on this pack and adds
/// seconds before a story starts. The box switches itself off after five idle
/// minutes, so one boot is roughly one session, and once per session per
/// figure is the cadence. It also needs no clock, which this box does not
/// have.
///
/// A figure is remembered when its answer *arrives*, whatever the answer was.
/// A server that was unreachable a moment ago is unreachable for the rest of a
/// five-minute session, and asking it again on the next placement spends the
/// radio to be told the same thing.
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

    /// Records that this figure has been asked about. Asking twice is not an
    /// error and does not push anything else out.
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
    /// Exists for the console: a bench that has just changed a file on the
    /// server needs to see the box notice, without power-cycling it first.
    pub fn forget_all(&mut self) {
        self.ruids.clear();
    }
}

/// How long a figure waits on the server before the box plays what it has.
///
/// The question costs an association, and an association that is *not* going
/// to happen costs the longest: a box carried out of range of its network
/// still tries, and the DHCP wait alone is 20 s. A child holding a figure whose
/// story is already on the card must not be made to wait that out.
///
/// Measured on 2026-09-14: a successful probe takes 6.9 s (3.1 s to associate
/// and lease, 3.8 s for TLS and the request), so this leaves 3.1 s of margin
/// over the good case — tighter than it should be, and recorded rather than
/// changed.
pub const PATIENCE_MS: u64 = 10_000;

/// What the server said about a figure's length, reduced to what the box can
/// act on.
///
/// `Nothing` is every answer that is not a stated length — no story, no
/// length, unreachable, the radio would not come up, the patience ran out.
/// The box does the same with each of them: plays the card. The console keeps
/// the difference, printed where it is known.
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

/// The conversation between the task that owns the card and the task that
/// asks the server, as one question at a time.
///
/// **Each question settles at most once.** That is the whole of its safety:
/// an answer that arrives after the patience ran out, or after an earlier
/// answer already settled the question, comes back as `None` and never
/// reaches the code that would act on it. The rule used to be spread across
/// two tasks writing the same statics, and a late "stale" slipped through it.
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

    /// A question has been put about this figure, at `now_ms`. Replaces any
    /// question still open: the reducer only ever waits on one figure.
    pub fn asked(&mut self, ruid: u64, now_ms: u64) {
        self.state = State::Asking {
            ruid,
            asked_ms: now_ms,
        };
    }

    /// The figure being asked about has left the plate. Ends the question
    /// without settling it: nobody is waiting for the answer any more, and one
    /// that arrives afterwards is late — even if the same figure is back by
    /// then, because it will be asked about afresh.
    pub fn withdrawn(&mut self) {
        self.state = State::Idle;
    }

    /// The clock has been read with no answer yet.
    ///
    /// Takes the reading rather than an interval, for the reason
    /// [`crate::Handshake::polled`] does: a caller cannot know how long its
    /// own wait took.
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

    /// The real numbers from 2026-09-13: `1d2e3f50500304e0` is 37,912,939
    /// bytes on the server and the same on the card.
    #[test]
    fn a_length_that_matches_is_current() {
        assert!(!is_stale(&sidecar(37_912_939), Probed::Length(37_912_939)));
    }

    #[test]
    fn a_length_that_differs_is_stale_whichever_way_it_moved() {
        assert!(is_stale(&sidecar(37_912_939), Probed::Length(37_912_940)));
        assert!(is_stale(&sidecar(37_912_939), Probed::Length(1_024)));
    }

    /// The case that decides whether this feature is safe to ship. A box
    /// carried out of range of its server must keep playing what it has.
    #[test]
    fn a_server_with_no_story_never_discards_the_one_on_the_card() {
        assert!(!is_stale(&sidecar(37_912_939), Probed::NoContent));
    }

    #[test]
    fn an_answer_without_a_length_changes_nothing() {
        assert!(!is_stale(&sidecar(37_912_939), Probed::Unstated));
    }

    /// An etag on the sidecar is not consulted: this server sends none, and a
    /// stale etag left over from an earlier design must not outvote a length
    /// that was measured this minute.
    #[test]
    fn the_recorded_etag_does_not_enter_into_it() {
        let with_etag = Sidecar {
            length: 4_096,
            etag: Some(ETag::try_from("\"v1\"").unwrap()),
        };
        assert!(is_stale(&with_etag, Probed::Length(8_192)));
        assert!(!is_stale(&with_etag, Probed::Length(4_096)));
    }

    #[test]
    fn a_figure_asked_about_once_is_not_asked_again() {
        let mut asked = Asked::new();
        assert!(!asked.contains(0x1D2E_3F50_5003_04E0));
        asked.remember(0x1D2E_3F50_5003_04E0);
        assert!(asked.contains(0x1D2E_3F50_5003_04E0));
    }

    #[test]
    fn remembering_the_same_figure_twice_costs_no_room() {
        let mut asked = Asked::new();
        for _ in 0..REMEMBERED * 2 {
            asked.remember(1);
        }
        asked.remember(2);
        assert!(asked.contains(1), "one crowding itself out is the bug");
        assert!(asked.contains(2));
    }

    /// The ninth figure of a session costs the first one its place, and the
    /// cost of that is one extra probe. Losing the *newest* instead would
    /// re-ask the figure currently in a child's hand, every time.
    #[test]
    fn a_ninth_figure_pushes_out_the_oldest() {
        let mut asked = Asked::new();
        for ruid in 0..REMEMBERED as u64 {
            asked.remember(ruid);
        }
        asked.remember(99);
        assert!(!asked.contains(0));
        assert!(asked.contains(1));
        assert!(asked.contains(99));
    }

    /// A bench that has just replaced a file on the server needs the box to
    /// notice without a power cycle.
    #[test]
    fn forgetting_makes_every_figure_ask_again() {
        let mut asked = Asked::new();
        asked.remember(7);
        asked.forget_all();
        assert!(!asked.contains(7));
    }

    const FIGURE: u64 = 0x1D2E_3F50_5003_04E0;
    const OTHER: u64 = 0x1E2F_4051_5003_04E0;

    #[test]
    fn an_answer_to_the_open_question_settles_it() {
        let mut r = Revalidation::new();
        r.asked(FIGURE, 0);
        assert_eq!(
            r.answered(FIGURE, Answer::Length(37_912_939)),
            Some(Settled {
                ruid: FIGURE,
                answer: Answer::Length(37_912_939)
            })
        );
    }

    #[test]
    fn a_question_nobody_answers_settles_as_nothing_when_patience_runs_out() {
        let mut r = Revalidation::new();
        r.asked(FIGURE, 0);
        assert_eq!(r.polled(9_999), None);
        assert_eq!(
            r.polled(10_000),
            Some(Settled {
                ruid: FIGURE,
                answer: Answer::Nothing
            })
        );
    }

    /// The defect this machine exists for. Before it, a real answer arriving
    /// after the patience ran out still reached `freshness_of`, which armed
    /// the stale flag for a figure already playing its card copy — and the
    /// next download of that file then started from zero, truncating it.
    #[test]
    fn an_answer_after_patience_ran_out_is_ignored() {
        let mut r = Revalidation::new();
        r.asked(FIGURE, 0);
        r.polled(10_000);
        assert_eq!(r.answered(FIGURE, Answer::Length(1_024)), None);
    }

    /// A figure lifted and put back mid-question raises a second probe; the
    /// first answer settles the question and the second belongs to nobody.
    #[test]
    fn an_answer_after_the_question_settled_is_ignored() {
        let mut r = Revalidation::new();
        r.asked(FIGURE, 0);
        r.answered(FIGURE, Answer::Length(37_912_939));
        assert_eq!(r.answered(FIGURE, Answer::Length(1_024)), None);
    }

    #[test]
    fn an_answer_about_another_figure_does_not_settle_the_question() {
        let mut r = Revalidation::new();
        r.asked(FIGURE, 0);
        assert_eq!(r.answered(OTHER, Answer::Nothing), None);
        assert_eq!(
            r.answered(FIGURE, Answer::Nothing),
            Some(Settled {
                ruid: FIGURE,
                answer: Answer::Nothing
            })
        );
    }

    #[test]
    fn a_new_question_replaces_the_open_one() {
        let mut r = Revalidation::new();
        r.asked(FIGURE, 0);
        r.asked(OTHER, 100);
        assert_eq!(r.answered(FIGURE, Answer::Length(1_024)), None);
    }

    #[test]
    fn patience_is_counted_from_the_latest_question() {
        let mut r = Revalidation::new();
        r.asked(FIGURE, 0);
        r.asked(OTHER, 5_000);
        assert_eq!(r.polled(10_000), None);
        assert_eq!(
            r.polled(15_000),
            Some(Settled {
                ruid: OTHER,
                answer: Answer::Nothing
            })
        );
    }

    /// Found in review, 2026-09-23. A figure lifted and put back within one
    /// pass of the media loop, as its answer arrives: the answer was settled
    /// against the figure already back on the plate, remembered and judged —
    /// arming the stale flag — before the reducer had even heard it was back.
    /// The reducer then played the card copy straight away, because the figure
    /// now counted as asked. A lift ends the question, so that answer is late.
    #[test]
    fn an_answer_after_the_figure_was_lifted_is_ignored() {
        let mut r = Revalidation::new();
        r.asked(FIGURE, 0);
        r.withdrawn();
        assert_eq!(r.answered(FIGURE, Answer::Length(1_024)), None);
    }

    /// Nobody is waiting for a lifted figure, so its patience running out is
    /// not news — and printing it sent a bench reader looking at a figure that
    /// was no longer there.
    #[test]
    fn a_lifted_figure_never_runs_out_of_patience() {
        let mut r = Revalidation::new();
        r.asked(FIGURE, 0);
        r.withdrawn();
        assert_eq!(r.polled(10_000), None);
    }

    #[test]
    fn a_clock_reading_before_the_ask_does_not_expire_it() {
        let mut r = Revalidation::new();
        r.asked(FIGURE, 20_000);
        assert_eq!(r.polled(0), None);
    }

    #[test]
    fn polls_with_nothing_asked_do_nothing() {
        let mut r = Revalidation::new();
        assert_eq!(r.polled(0), None);
        assert_eq!(r.polled(1_000_000), None);
    }

    /// The media loop is where this is polled, and a pass takes as long as
    /// whatever blocked the executor. Measured on 2026-09-23: a 10 ms timer
    /// came back every ~106 ms under a story. The patience must be wall clock
    /// whatever the gap.
    #[test]
    fn the_patience_is_wall_clock_however_slowly_the_caller_polls() {
        const GAP_MS: u64 = 106;
        let mut r = Revalidation::new();
        r.asked(FIGURE, 0);
        let mut now = 0;
        while r.polled(now).is_none() {
            now += GAP_MS;
            assert!(now <= 10_000 + GAP_MS, "still waiting at {now} ms");
        }
        assert!(now >= 10_000, "gave up early, at {now} ms");
    }
}
