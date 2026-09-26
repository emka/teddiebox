//! How a download ended, kept until the task that asked for it reads it.
//!
//! The download runs in the network task and finishes in the media task,
//! where the reducer decides what to do about it. Neither can call the other,
//! so the result waits here, labelled with the figure it belongs to.

/// How a download ended, only as detailed as the reducer needs.
///
/// The console prints the exact error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The bytes are on the card.
    Completed,
    /// The server could not be reached, or could not be asked.
    Unreachable,
    /// The server was reached and has nothing filed under that figure.
    NoContent,
    /// The access point turned the box away: the card's passphrase is not its
    /// one.
    Refused,
}

/// The last download's outcome, and the figure it is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Outcomes {
    active: u64,
    pending: Option<(Outcome, u64)>,
}

impl Outcomes {
    pub const fn new() -> Self {
        Self {
            active: 0,
            pending: None,
        }
    }

    /// Names the figure later reports are about.
    ///
    /// A report already waiting keeps its own figure, so a result is never
    /// credited to a request queued after it.
    pub fn start(&mut self, ruid: u64) {
        self.active = ruid;
    }

    /// Records how the download ended, replacing anything not yet read.
    pub fn report(&mut self, outcome: Outcome) {
        self.pending = Some((outcome, self.active));
    }

    /// Records how the download ended, unless something already has.
    ///
    /// The network task knows *why* a fetch failed; the media task only knows
    /// the file stopped short. Deferring keeps the precise reason, which the
    /// box announces differently. Returns whether it recorded anything.
    pub fn report_if_silent(&mut self, outcome: Outcome) -> bool {
        if self.pending.is_some() {
            return false;
        }
        self.report(outcome);
        true
    }

    /// Takes the waiting outcome and its figure, if there is one.
    pub fn take(&mut self) -> Option<(Outcome, u64)> {
        self.pending.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIGURE: u64 = 0xE0_04_03_50_1A_2B_3C_4D;
    const OTHER: u64 = 0xE0_04_03_50_99_88_77_66;

    #[test]
    fn a_report_is_labelled_with_the_figure_that_started_the_download() {
        let mut outcomes = Outcomes::new();
        outcomes.start(FIGURE);
        outcomes.report(Outcome::Completed);
        assert_eq!(outcomes.take(), Some((Outcome::Completed, FIGURE)));
    }

    #[test]
    fn starting_another_download_does_not_relabel_an_unread_report() {
        let mut outcomes = Outcomes::new();
        outcomes.start(FIGURE);
        outcomes.report(Outcome::NoContent);
        outcomes.start(OTHER);
        assert_eq!(outcomes.take(), Some((Outcome::NoContent, FIGURE)));
    }

    #[test]
    fn a_report_replaces_an_unread_one() {
        let mut outcomes = Outcomes::new();
        outcomes.start(FIGURE);
        outcomes.report(Outcome::Unreachable);
        outcomes.report(Outcome::Refused);
        assert_eq!(outcomes.take(), Some((Outcome::Refused, FIGURE)));
    }

    #[test]
    fn a_report_if_silent_leaves_an_unread_report_alone() {
        let mut outcomes = Outcomes::new();
        outcomes.start(FIGURE);
        outcomes.report(Outcome::NoContent);
        assert!(!outcomes.report_if_silent(Outcome::Unreachable));
        assert_eq!(outcomes.take(), Some((Outcome::NoContent, FIGURE)));
    }

    #[test]
    fn a_report_if_silent_records_when_nothing_is_waiting() {
        let mut outcomes = Outcomes::new();
        outcomes.start(FIGURE);
        assert!(outcomes.report_if_silent(Outcome::Unreachable));
        assert_eq!(outcomes.take(), Some((Outcome::Unreachable, FIGURE)));
    }

    #[test]
    fn taking_the_outcome_empties_the_slot() {
        let mut outcomes = Outcomes::new();
        outcomes.start(FIGURE);
        outcomes.report(Outcome::Completed);
        outcomes.take();
        assert_eq!(outcomes.take(), None);
    }
}
