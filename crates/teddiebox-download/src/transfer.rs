//! One download's handoff between the task that fetches it and the task that
//! owns the card, as a state machine with no I/O.
//!
//! The network task produces the bytes; only the media task may write them to
//! the card, and it is busy for seconds at a time while a story plays. Neither
//! can wait for the other, so they meet here: each makes short calls on one
//! shared value and reads back what to do next. Keeping it here means how a
//! download starts, finishes and reports can be tested on the host.
//!
//! The bytes themselves travel through a separate pipe; this holds only what
//! is known *about* them.

use crate::Sidecar;
use crate::{is_whole, place, CardSays, ContentPath, Landing, Outcome, Outcomes, Placement};
use teddiebox_cloud::ETag;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Nothing is downloading.
    Idle,
    /// The producer is waiting for the card's owner to say what it holds.
    Asking,
    /// The card has answered; the producer is building its request.
    Planned,
    /// The response head has arrived and bytes are flowing.
    Running,
    /// The producer has stopped. What is still in the pipe is the last of it.
    Ended,
}

/// What the card holds of the file, as its owner found it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// The answer that decides what the request asks for.
    pub says: CardSays,
    /// How many bytes the content file held.
    pub on_card: u32,
    /// How long the sidecar promised the whole file would be, if there was
    /// one. Lets a resume of *this* file be told from one against a file the
    /// server has since replaced.
    pub expected: Option<u32>,
    /// What the sidecar says a resume should be validated against.
    pub resume_etag: Option<ETag>,
}

/// What the response head said about the body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Head {
    /// Where the server said this body belongs. Zero means it declined the
    /// range.
    pub at: u32,
    /// How long the server says the whole file is.
    pub total: Option<u32>,
    /// What a later resume can be validated against.
    pub etag: Option<ETag>,
}

/// What the card's owner should do on this pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Work {
    /// Nothing is downloading.
    Idle,
    /// Read what the card holds of this file and answer with
    /// [`Transfer::planned`].
    Plan(ContentPath),
    /// The request is in flight; nothing to write yet.
    Wait,
    /// The request ended before any byte arrived. Nothing is opened, so no
    /// cache entry is created or truncated for it.
    Discard,
    /// Open the file where the body belongs, write `sidecar` if there is one,
    /// then drain.
    Open {
        path: ContentPath,
        placement: Placement,
        /// Where this body starts in the file.
        at: u32,
        /// Only when starting over, and only when the server gave a length: a
        /// resumed download continues the file the existing sidecar describes.
        sidecar: Option<Sidecar>,
    },
    /// Move what is in the pipe onto the open file, then call
    /// [`Transfer::finish`].
    Drain,
    /// Empty the pipe without writing, then call [`Transfer::finish`].
    ///
    /// A write to the file failed. Appending anything after it would leave a
    /// gap in the middle of the story that a later resume would build on;
    /// stopping keeps what is on the card a clean prefix of the file.
    Drop,
}

/// How a download that reached the card came out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Finished {
    /// Whether the file now holds the whole story.
    pub whole: bool,
    /// Where this body started in the file.
    pub at: u32,
    /// How many bytes this body put on the card.
    pub written: u32,
    /// How long the server said the whole file is.
    pub total: Option<u32>,
}

/// One download, as both tasks see it.
///
/// The fields outlive the phase on purpose: a card failure makes the transfer
/// idle while the producer is still running, and when that producer ends, its
/// path and head are what the next open needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transfer {
    phase: Phase,
    path: ContentPath,
    plan: Plan,
    head: Head,
    /// Bytes the producer has handed to the pipe.
    sent: u32,
    /// Set when the figure is lifted or the card cannot be written, so the
    /// producer stops rather than filling a pipe nobody drains.
    aborted: bool,
    /// Set when a write to the card failed. The bytes of that write left the
    /// pipe without reaching the card, so the card can never catch up with
    /// what was sent.
    broken: bool,
    /// The figure the next request is for.
    requested: u64,
    /// The figure whose download this is, from the ask that started it.
    figure: u64,
    outcomes: Outcomes,
}

impl Default for Transfer {
    fn default() -> Self {
        Self::new()
    }
}

impl Transfer {
    pub const fn new() -> Self {
        Self {
            phase: Phase::Idle,
            path: ContentPath {
                directory: 0,
                file: 0,
            },
            plan: Plan {
                says: CardSays::Nothing,
                on_card: 0,
                expected: None,
                resume_etag: None,
            },
            head: Head {
                at: 0,
                total: None,
                etag: None,
            },
            sent: 0,
            aborted: false,
            broken: false,
            requested: 0,
            figure: 0,
            outcomes: Outcomes::new(),
        }
    }

    // The producer's side.

    /// Names the figure the next request is for.
    ///
    /// Call it before anything can end the fetch, including a failure to
    /// connect, so that failure is credited to it. A download still being
    /// written keeps its own figure; the new one takes this figure only when
    /// its ask is accepted.
    pub fn start(&mut self, ruid: u64) {
        self.requested = ruid;
    }

    /// Asks the card's owner what it holds of `path`, and returns whether it
    /// did.
    ///
    /// Refused while the last download is still being written to the card:
    /// the new one would reuse its pipe and its open file. The producer asks
    /// again until the card's owner has finished, or gives up.
    ///
    /// Asking again while still waiting is harmless: nothing has been sent
    /// before the card answers.
    pub fn ask(&mut self, path: ContentPath) -> bool {
        if matches!(self.phase, Phase::Running | Phase::Ended) {
            return false;
        }
        self.figure = self.requested;
        self.path = path;
        self.sent = 0;
        self.head = Head {
            at: 0,
            total: None,
            etag: None,
        };
        self.phase = Phase::Asking;
        true
    }

    /// The card's answer, once its owner has given one.
    pub fn card_answer(&self) -> Option<CardSays> {
        (self.phase == Phase::Planned).then_some(self.plan.says)
    }

    /// The card holds the whole file, so there is nothing to fetch.
    pub fn holds_all(&mut self) {
        self.outcomes.report(Outcome::Completed, self.figure);
        self.phase = Phase::Idle;
    }

    /// The card did not answer in time. What is cached is left alone, and so
    /// is a previous download still being written.
    pub fn gave_up(&mut self) {
        self.outcomes
            .report_if_silent(Outcome::Unreachable, self.requested);
        if self.phase == Phase::Asking {
            self.phase = Phase::Idle;
        }
    }

    /// The request is about to be sent. A lift from before it no longer
    /// applies.
    pub fn fetching(&mut self) {
        self.aborted = false;
    }

    /// What a resume should be validated against.
    pub fn resume_etag(&self) -> Option<ETag> {
        self.plan.resume_etag.clone()
    }

    /// The response head has arrived. A total of zero counts as none.
    pub fn headers(&mut self, head: Head) {
        self.head = Head {
            total: head.total.filter(|&total| total != 0),
            ..head
        };
        self.phase = Phase::Running;
    }

    /// Whether the producer should stop sending.
    pub fn should_stop(&self) -> bool {
        self.aborted
    }

    /// The producer handed `bytes` more to the pipe.
    pub fn sent(&mut self, bytes: u32) {
        self.sent = self.sent.wrapping_add(bytes);
    }

    /// The producer has stopped, and why, if it failed.
    ///
    /// A failure's reason is recorded here because only the producer knows
    /// it; whether the story is complete is decided at the card.
    pub fn ended(&mut self, failure: Option<Outcome>) {
        self.phase = Phase::Ended;
        if let Some(why) = failure {
            self.outcomes.report(why, self.figure);
        }
    }

    /// The radio could not be brought up for this fetch.
    ///
    /// Reported only when no download is in flight and nothing else has
    /// reported, since otherwise that download reports for itself. Returns
    /// whether it recorded anything.
    pub fn could_not_connect(&mut self, why: Outcome) -> bool {
        self.phase == Phase::Idle && self.outcomes.report_if_silent(why, self.requested)
    }

    // The card owner's side.

    /// What to do on this pass. `writing` is whether a file is open for this
    /// download.
    pub fn next(&mut self, writing: bool) -> Work {
        match self.phase {
            Phase::Idle => Work::Idle,
            Phase::Asking => Work::Plan(self.path),
            Phase::Planned => Work::Wait,
            Phase::Running | Phase::Ended if writing && self.broken => Work::Drop,
            Phase::Running | Phase::Ended if writing => Work::Drain,
            Phase::Ended if self.sent == 0 => {
                self.phase = Phase::Idle;
                Work::Discard
            }
            Phase::Running | Phase::Ended => {
                // The offset the server sent decides whether the partial file
                // is continued or replaced. Getting it wrong would join the
                // start of a story onto its middle, at a length that looks
                // correct.
                let placement = place(&Landing {
                    offset: self.head.at,
                    length_on_card: self.plan.on_card,
                    expected: self.plan.expected,
                    total: self.head.total,
                });
                let sidecar = match placement {
                    Placement::Restart => self.head.total.map(|length| Sidecar {
                        length,
                        etag: self.head.etag.clone(),
                    }),
                    Placement::Continue | Placement::Refuse => None,
                };
                Work::Open {
                    path: self.path,
                    placement,
                    at: self.head.at,
                    sidecar,
                }
            }
        }
    }

    /// What the card holds, in answer to [`Work::Plan`].
    ///
    /// Ignored unless the producer is still asking: reading the card can take
    /// longer than the producer waits, and an answer kept after it gave up
    /// would leave the transfer waiting for a request that never comes. A
    /// promised length of zero counts as none.
    pub fn planned(&mut self, plan: Plan) {
        if self.phase != Phase::Asking {
            return;
        }
        self.plan = Plan {
            expected: plan.expected.filter(|&expected| expected != 0),
            ..plan
        };
        self.phase = Phase::Planned;
    }

    /// The file could not be opened, or the server's range does not fit it.
    ///
    /// Reported as `Unreachable`: a card problem is not the server's fault,
    /// but "reached and has nothing" would be wrong, and `Unreachable` makes
    /// the box try again next time.
    pub fn open_failed(&mut self) {
        self.outcomes.report(Outcome::Unreachable, self.figure);
        self.aborted = true;
        self.phase = Phase::Idle;
    }

    /// Writing to the open file failed. Reported as for
    /// [`Transfer::open_failed`]. The producer is stopped, the rest of the
    /// pipe is dropped rather than written, and the file is finished once the
    /// producer has ended.
    pub fn write_failed(&mut self) {
        self.outcomes.report(Outcome::Unreachable, self.figure);
        self.aborted = true;
        self.broken = true;
    }

    /// Ends the download once the producer has stopped and nothing more will
    /// reach the card.
    ///
    /// `written` is how many bytes of this body reached the card. Normally
    /// that must have caught up with what was sent, or the file would lose
    /// what was still in the pipe. After a failed write it never will, so the
    /// download ends as soon as the producer does.
    ///
    /// A stopped transfer is not necessarily a whole story, so this compares
    /// where the body started plus what was written against the server's
    /// length. A short one reports `Unreachable` unless it was abandoned, in
    /// which case the reducer has already moved on.
    pub fn finish(&mut self, written: u32) -> Option<Finished> {
        if self.phase != Phase::Ended || (written < self.sent && !self.broken) {
            return None;
        }
        let whole = is_whole(self.head.at, written, self.head.total);
        if whole {
            self.outcomes.report(Outcome::Completed, self.figure);
        } else if !self.aborted {
            self.outcomes
                .report_if_silent(Outcome::Unreachable, self.figure);
        }
        self.phase = Phase::Idle;
        self.aborted = false;
        self.broken = false;
        Some(Finished {
            whole,
            at: self.head.at,
            written,
            total: self.head.total,
        })
    }

    /// The figure was lifted; nobody is waiting for this download.
    pub fn abort(&mut self) {
        self.aborted = true;
    }

    /// Takes how the last download ended, and the figure it was for.
    pub fn take_outcome(&mut self) -> Option<(Outcome, u64)> {
        self.outcomes.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIGURE: u64 = 0xE0_04_03_50_1A_2B_3C_4D;
    const OTHER_FIGURE: u64 = 0xE0_04_03_50_99_88_77_66;
    const PATH: ContentPath = ContentPath {
        directory: 0x4D3C_2B1A,
        file: 0x5003_04E0,
    };
    const OTHER_PATH: ContentPath = ContentPath {
        directory: 0x6677_8899,
        file: 0x5003_04E0,
    };

    fn etag() -> Option<ETag> {
        ETag::try_from("\"v1\"").ok()
    }

    fn nothing_cached() -> Plan {
        Plan {
            says: CardSays::Nothing,
            on_card: 0,
            expected: None,
            resume_etag: None,
        }
    }

    fn half_cached() -> Plan {
        Plan {
            says: CardSays::Holds(1_000),
            on_card: 1_000,
            expected: Some(4_000),
            resume_etag: etag(),
        }
    }

    fn whole_file(total: Option<u32>) -> Head {
        Head {
            at: 0,
            total,
            etag: etag(),
        }
    }

    fn the_rest() -> Head {
        Head {
            at: 1_000,
            total: Some(4_000),
            etag: etag(),
        }
    }

    /// A transfer the card has planned and the server has started answering.
    fn running(plan: Plan, head: Head) -> Transfer {
        let mut transfer = Transfer::new();
        transfer.start(FIGURE);
        transfer.ask(PATH);
        transfer.planned(plan);
        transfer.fetching();
        transfer.headers(head);
        transfer
    }

    #[test]
    fn an_asked_transfer_asks_the_media_task_to_read_the_card() {
        let mut transfer = Transfer::new();
        transfer.ask(PATH);
        assert_eq!(transfer.next(false), Work::Plan(PATH));
    }

    #[test]
    fn the_card_answer_reaches_the_producer_once_planned() {
        let mut transfer = Transfer::new();
        transfer.ask(PATH);
        assert_eq!(transfer.card_answer(), None);
        transfer.planned(half_cached());
        assert_eq!(transfer.card_answer(), Some(CardSays::Holds(1_000)));
    }

    #[test]
    fn a_card_answer_after_the_producer_gave_up_leaves_the_transfer_idle() {
        let mut transfer = Transfer::new();
        transfer.ask(PATH);
        transfer.gave_up();
        transfer.planned(half_cached());
        assert_eq!(transfer.next(false), Work::Idle);
        assert_eq!(transfer.card_answer(), None);
    }

    #[test]
    fn giving_up_reports_unreachable() {
        let mut transfer = Transfer::new();
        transfer.start(FIGURE);
        transfer.ask(PATH);
        transfer.gave_up();
        assert_eq!(
            transfer.take_outcome(),
            Some((Outcome::Unreachable, FIGURE))
        );
    }

    #[test]
    fn a_card_that_holds_everything_reports_completed_without_a_request() {
        let mut transfer = Transfer::new();
        transfer.start(FIGURE);
        transfer.ask(PATH);
        transfer.holds_all();
        assert_eq!(transfer.next(false), Work::Idle);
        assert_eq!(transfer.take_outcome(), Some((Outcome::Completed, FIGURE)));
    }

    #[test]
    fn the_producer_waits_while_the_request_is_in_flight() {
        let mut transfer = Transfer::new();
        transfer.ask(PATH);
        transfer.planned(nothing_cached());
        assert_eq!(transfer.next(false), Work::Wait);
    }

    #[test]
    fn a_resume_is_sent_the_etag_the_sidecar_holds() {
        let mut transfer = Transfer::new();
        transfer.ask(PATH);
        transfer.planned(half_cached());
        assert_eq!(transfer.resume_etag(), etag());
    }

    #[test]
    fn a_request_that_ends_having_sent_nothing_is_discarded_not_opened() {
        let mut transfer = Transfer::new();
        transfer.start(FIGURE);
        transfer.ask(PATH);
        transfer.planned(nothing_cached());
        transfer.ended(Some(Outcome::Unreachable));
        assert_eq!(transfer.next(false), Work::Discard);
        assert_eq!(transfer.next(false), Work::Idle);
    }

    #[test]
    fn a_fresh_download_opens_from_the_start_with_the_servers_sidecar() {
        let mut transfer = running(nothing_cached(), whole_file(Some(4_000)));
        assert_eq!(
            transfer.next(false),
            Work::Open {
                path: PATH,
                placement: Placement::Restart,
                at: 0,
                sidecar: Some(Sidecar {
                    length: 4_000,
                    etag: etag(),
                }),
            }
        );
    }

    #[test]
    fn a_fresh_download_with_no_length_writes_no_sidecar() {
        let mut transfer = running(nothing_cached(), whole_file(None));
        assert_eq!(
            transfer.next(false),
            Work::Open {
                path: PATH,
                placement: Placement::Restart,
                at: 0,
                sidecar: None,
            }
        );
    }

    #[test]
    fn a_length_of_zero_counts_as_no_length() {
        let mut transfer = running(nothing_cached(), whole_file(Some(0)));
        assert!(matches!(
            transfer.next(false),
            Work::Open { sidecar: None, .. }
        ));
    }

    #[test]
    fn a_resume_continues_the_file_and_keeps_its_sidecar() {
        let mut transfer = running(half_cached(), the_rest());
        assert_eq!(
            transfer.next(false),
            Work::Open {
                path: PATH,
                placement: Placement::Continue,
                at: 1_000,
                sidecar: None,
            }
        );
    }

    #[test]
    fn a_range_of_a_file_that_has_changed_is_refused() {
        let changed = Head {
            at: 1_000,
            total: Some(5_000),
            etag: etag(),
        };
        let mut transfer = running(half_cached(), changed);
        assert!(matches!(
            transfer.next(false),
            Work::Open {
                placement: Placement::Refuse,
                ..
            }
        ));
    }

    #[test]
    fn a_sidecar_promising_zero_bytes_counts_as_no_sidecar() {
        let plan = Plan {
            expected: Some(0),
            ..half_cached()
        };
        let different_total = Head {
            at: 1_000,
            total: Some(5_000),
            etag: etag(),
        };
        let mut transfer = running(plan, different_total);
        assert!(matches!(
            transfer.next(false),
            Work::Open {
                placement: Placement::Continue,
                ..
            }
        ));
    }

    #[test]
    fn an_open_file_is_drained() {
        let mut transfer = running(nothing_cached(), whole_file(Some(4_000)));
        transfer.sent(512);
        assert_eq!(transfer.next(true), Work::Drain);
    }

    #[test]
    fn a_download_is_not_finished_while_bytes_remain_in_the_pipe() {
        let mut transfer = running(nothing_cached(), whole_file(Some(4_000)));
        transfer.sent(4_000);
        transfer.ended(None);
        assert_eq!(transfer.finish(3_488), None);
    }

    #[test]
    fn a_download_is_not_finished_while_the_producer_is_still_sending() {
        let mut transfer = running(nothing_cached(), whole_file(Some(4_000)));
        transfer.sent(2_000);
        assert_eq!(transfer.finish(2_000), None);
    }

    #[test]
    fn a_whole_download_reports_completed_only_once_it_is_on_the_card() {
        let mut transfer = running(nothing_cached(), whole_file(Some(4_000)));
        transfer.sent(4_000);
        transfer.ended(None);
        assert_eq!(transfer.take_outcome(), None);
        assert_eq!(
            transfer.finish(4_000),
            Some(Finished {
                whole: true,
                at: 0,
                written: 4_000,
                total: Some(4_000),
            })
        );
        assert_eq!(transfer.take_outcome(), Some((Outcome::Completed, FIGURE)));
        assert_eq!(transfer.next(false), Work::Idle);
    }

    #[test]
    fn a_resumed_download_is_whole_when_it_reaches_the_servers_length() {
        let mut transfer = running(half_cached(), the_rest());
        transfer.sent(3_000);
        transfer.ended(None);
        assert!(matches!(
            transfer.finish(3_000),
            Some(Finished { whole: true, .. })
        ));
    }

    #[test]
    fn a_download_that_stopped_short_reports_unreachable() {
        let mut transfer = running(nothing_cached(), whole_file(Some(4_000)));
        transfer.sent(2_000);
        transfer.ended(None);
        assert!(matches!(
            transfer.finish(2_000),
            Some(Finished { whole: false, .. })
        ));
        assert_eq!(
            transfer.take_outcome(),
            Some((Outcome::Unreachable, FIGURE))
        );
    }

    #[test]
    fn a_short_download_keeps_the_reason_the_producer_gave() {
        let mut transfer = running(nothing_cached(), whole_file(Some(4_000)));
        transfer.sent(2_000);
        transfer.ended(Some(Outcome::NoContent));
        transfer.finish(2_000);
        assert_eq!(transfer.take_outcome(), Some((Outcome::NoContent, FIGURE)));
    }

    #[test]
    fn an_abandoned_download_reports_nothing() {
        let mut transfer = running(nothing_cached(), whole_file(Some(4_000)));
        transfer.sent(2_000);
        transfer.abort();
        transfer.ended(None);
        transfer.finish(2_000);
        assert_eq!(transfer.take_outcome(), None);
    }

    #[test]
    fn a_lift_stops_the_producer() {
        let mut transfer = running(nothing_cached(), whole_file(Some(4_000)));
        transfer.abort();
        assert!(transfer.should_stop());
    }

    #[test]
    fn a_new_fetch_forgets_an_earlier_abort() {
        let mut transfer = Transfer::new();
        transfer.abort();
        transfer.fetching();
        assert!(!transfer.should_stop());
    }

    #[test]
    fn a_finished_download_forgets_its_abort() {
        let mut transfer = running(nothing_cached(), whole_file(Some(4_000)));
        transfer.abort();
        transfer.ended(None);
        transfer.finish(0);
        assert!(!transfer.should_stop());
    }

    #[test]
    fn a_card_that_cannot_be_opened_stops_the_producer_and_reports_unreachable() {
        let mut transfer = running(nothing_cached(), whole_file(Some(4_000)));
        transfer.open_failed();
        assert!(transfer.should_stop());
        assert_eq!(transfer.next(false), Work::Idle);
        assert_eq!(
            transfer.take_outcome(),
            Some((Outcome::Unreachable, FIGURE))
        );
    }

    #[test]
    fn a_producer_that_ends_after_the_card_failed_is_opened_again_not_left_hanging() {
        let mut transfer = running(nothing_cached(), whole_file(Some(4_000)));
        transfer.sent(512);
        transfer.open_failed();
        transfer.ended(None);
        assert!(matches!(transfer.next(false), Work::Open { .. }));
    }

    #[test]
    fn a_card_that_cannot_be_written_stops_the_producer_and_reports_unreachable() {
        let mut transfer = running(nothing_cached(), whole_file(Some(4_000)));
        transfer.write_failed();
        assert!(transfer.should_stop());
        assert_eq!(
            transfer.take_outcome(),
            Some((Outcome::Unreachable, FIGURE))
        );
    }

    #[test]
    fn a_failure_to_connect_is_reported_while_nothing_is_in_flight() {
        let mut transfer = Transfer::new();
        transfer.start(FIGURE);
        assert!(transfer.could_not_connect(Outcome::Refused));
        assert_eq!(transfer.take_outcome(), Some((Outcome::Refused, FIGURE)));
    }

    #[test]
    fn a_failure_to_connect_is_not_reported_while_a_download_is_in_flight() {
        let mut transfer = running(nothing_cached(), whole_file(Some(4_000)));
        assert!(!transfer.could_not_connect(Outcome::Unreachable));
        assert_eq!(transfer.take_outcome(), None);
    }

    /// A transfer with an outcome from an earlier attempt still unread.
    fn with_unread_refusal() -> Transfer {
        let mut transfer = Transfer::new();
        transfer.start(FIGURE);
        transfer.could_not_connect(Outcome::Refused);
        transfer
    }

    #[test]
    fn a_whole_story_is_completed_even_if_the_producer_reported_a_failure() {
        let mut transfer = running(nothing_cached(), whole_file(Some(4_000)));
        transfer.sent(4_000);
        transfer.ended(Some(Outcome::Unreachable));
        transfer.finish(4_000);
        assert_eq!(transfer.take_outcome(), Some((Outcome::Completed, FIGURE)));
    }

    #[test]
    fn the_producers_reason_replaces_an_unread_outcome() {
        let mut transfer = with_unread_refusal();
        transfer.ask(PATH);
        transfer.planned(nothing_cached());
        transfer.ended(Some(Outcome::NoContent));
        assert_eq!(transfer.take_outcome(), Some((Outcome::NoContent, FIGURE)));
    }

    #[test]
    fn a_card_that_cannot_be_opened_replaces_an_unread_outcome() {
        let mut transfer = with_unread_refusal();
        transfer.ask(PATH);
        transfer.planned(nothing_cached());
        transfer.headers(whole_file(Some(4_000)));
        transfer.open_failed();
        assert_eq!(
            transfer.take_outcome(),
            Some((Outcome::Unreachable, FIGURE))
        );
    }

    #[test]
    fn a_card_that_cannot_be_written_replaces_an_unread_outcome() {
        let mut transfer = with_unread_refusal();
        transfer.ask(PATH);
        transfer.planned(nothing_cached());
        transfer.headers(whole_file(Some(4_000)));
        transfer.write_failed();
        assert_eq!(
            transfer.take_outcome(),
            Some((Outcome::Unreachable, FIGURE))
        );
    }

    #[test]
    fn a_card_that_holds_everything_replaces_an_unread_outcome() {
        let mut transfer = with_unread_refusal();
        transfer.ask(PATH);
        transfer.holds_all();
        assert_eq!(transfer.take_outcome(), Some((Outcome::Completed, FIGURE)));
    }

    #[test]
    fn giving_up_leaves_an_unread_outcome_alone() {
        let mut transfer = with_unread_refusal();
        transfer.ask(PATH);
        transfer.gave_up();
        assert_eq!(transfer.take_outcome(), Some((Outcome::Refused, FIGURE)));
    }

    #[test]
    fn a_failure_to_connect_leaves_an_unread_outcome_alone() {
        let mut transfer = with_unread_refusal();
        assert!(!transfer.could_not_connect(Outcome::Unreachable));
        assert_eq!(transfer.take_outcome(), Some((Outcome::Refused, FIGURE)));
    }

    #[test]
    fn a_new_ask_forgets_the_bytes_the_last_transfer_sent() {
        let mut transfer = running(nothing_cached(), whole_file(Some(4_000)));
        transfer.sent(512);
        transfer.ended(None);
        transfer.finish(512);
        transfer.ask(PATH);
        transfer.planned(nothing_cached());
        transfer.ended(Some(Outcome::Unreachable));
        assert_eq!(transfer.next(false), Work::Discard);
    }

    #[test]
    fn a_failed_write_finishes_once_the_producer_stops_despite_the_bytes_it_lost() {
        let mut transfer = running(nothing_cached(), whole_file(Some(4_000)));
        transfer.sent(1_024);
        // The second 512-byte chunk left the pipe but never reached the card.
        transfer.write_failed();
        transfer.ended(None);
        assert_eq!(
            transfer.finish(512),
            Some(Finished {
                whole: false,
                at: 0,
                written: 512,
                total: Some(4_000),
            })
        );
    }

    #[test]
    fn nothing_more_is_written_after_a_failed_write() {
        let mut transfer = running(nothing_cached(), whole_file(Some(4_000)));
        transfer.sent(1_024);
        transfer.write_failed();
        assert_eq!(transfer.next(true), Work::Drop);
    }

    /// A transfer whose producer has stopped while its last bytes are still
    /// in the pipe.
    fn still_being_written() -> Transfer {
        let mut transfer = running(nothing_cached(), whole_file(Some(4_000)));
        transfer.sent(4_000);
        transfer.ended(None);
        transfer
    }

    #[test]
    fn a_new_fetch_waits_while_the_last_one_is_still_being_written() {
        let mut transfer = still_being_written();
        transfer.ask(OTHER_PATH);
        assert_eq!(transfer.next(true), Work::Drain);
    }

    #[test]
    fn giving_up_on_a_new_fetch_leaves_the_last_one_being_written() {
        let mut transfer = still_being_written();
        transfer.ask(OTHER_PATH);
        transfer.gave_up();
        assert_eq!(transfer.next(true), Work::Drain);
    }

    #[test]
    fn a_new_fetch_is_asked_once_the_last_one_has_finished() {
        let mut transfer = still_being_written();
        transfer.finish(4_000);
        assert!(transfer.ask(OTHER_PATH));
        assert_eq!(transfer.next(false), Work::Plan(OTHER_PATH));
    }

    #[test]
    fn the_last_download_is_credited_to_its_own_figure_when_a_new_one_is_waiting() {
        let mut transfer = still_being_written();
        transfer.start(OTHER_FIGURE);
        transfer.ask(OTHER_PATH);
        transfer.finish(4_000);
        assert_eq!(transfer.take_outcome(), Some((Outcome::Completed, FIGURE)));
    }

    #[test]
    fn an_unread_outcome_for_another_figure_does_not_silence_a_new_fetch() {
        let mut transfer = Transfer::new();
        transfer.start(FIGURE);
        transfer.ask(PATH);
        transfer.planned(nothing_cached());
        transfer.ended(Some(Outcome::NoContent));
        assert_eq!(transfer.next(false), Work::Discard);

        transfer.start(OTHER_FIGURE);
        transfer.ask(OTHER_PATH);
        transfer.gave_up();
        assert_eq!(
            transfer.take_outcome(),
            Some((Outcome::Unreachable, OTHER_FIGURE))
        );
    }
}
