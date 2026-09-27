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
    pending: Option<(Outcome, u64)>,
}

impl Outcomes {
    pub const fn new() -> Self {
        Self { pending: None }
    }

    /// Records how the download for `ruid` ended, replacing anything not yet
    /// read.
    pub fn report(&mut self, outcome: Outcome, ruid: u64) {
        self.pending = Some((outcome, ruid));
    }

    /// Records how the download for `ruid` ended, unless something already
    /// has.
    ///
    /// The network task knows *why* a fetch failed; the media task only knows
    /// the file stopped short. Deferring keeps the precise reason, which the
    /// box announces differently. Returns whether it recorded anything.
    pub fn report_if_silent(&mut self, outcome: Outcome, ruid: u64) -> bool {
        if self.pending.is_some() {
            return false;
        }
        self.report(outcome, ruid);
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

    #[test]
    fn a_report_carries_the_figure_it_is_about() {
        let mut outcomes = Outcomes::new();
        outcomes.report(Outcome::Completed, FIGURE);
        assert_eq!(outcomes.take(), Some((Outcome::Completed, FIGURE)));
    }

    #[test]
    fn a_report_replaces_an_unread_one() {
        let mut outcomes = Outcomes::new();
        outcomes.report(Outcome::Unreachable, FIGURE);
        outcomes.report(Outcome::Refused, FIGURE);
        assert_eq!(outcomes.take(), Some((Outcome::Refused, FIGURE)));
    }

    #[test]
    fn a_report_if_silent_leaves_an_unread_report_alone() {
        let mut outcomes = Outcomes::new();
        outcomes.report(Outcome::NoContent, FIGURE);
        assert!(!outcomes.report_if_silent(Outcome::Unreachable, FIGURE));
        assert_eq!(outcomes.take(), Some((Outcome::NoContent, FIGURE)));
    }

    #[test]
    fn a_report_if_silent_records_when_nothing_is_waiting() {
        let mut outcomes = Outcomes::new();
        assert!(outcomes.report_if_silent(Outcome::Unreachable, FIGURE));
        assert_eq!(outcomes.take(), Some((Outcome::Unreachable, FIGURE)));
    }

    #[test]
    fn taking_the_outcome_empties_the_slot() {
        let mut outcomes = Outcomes::new();
        outcomes.report(Outcome::Completed, FIGURE);
        outcomes.take();
        assert_eq!(outcomes.take(), None);
    }
}
