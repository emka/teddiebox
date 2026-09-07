//! When the download should give the audio some room.
//!
//! Measured on the bench: the box downloads at roughly 42 KB/s and Opus in a
//! Tonie plays at about 8, so a download outruns playback by five to one. It
//! does not need to run all the time, and it costs something when it does —
//! Wi-Fi receive work makes the media task late, and a late media task lets the
//! audio DMA reach the end of its descriptor chain and stop. That is survivable
//! now, but every restart is an audible glitch.
//!
//! So while a story is playing, the download runs only when the lead is getting
//! thin, and stops once it is comfortable again. Nothing plays: it runs flat
//! out.
//!
//! **Hysteresis is the whole point.** A single threshold makes the download
//! start and stop on every page boundary, which is the worst of both — the
//! Wi-Fi traffic never settles and the lead never grows. Two thresholds mean it
//! fetches in bursts and is quiet in between.

use crate::units::Pages;

/// Whether the download is allowed to read from the network right now.
///
/// Holds one bit of state because the answer depends on what it said last:
/// between the two thresholds the previous decision stands, and that is what
/// stops it flapping.
/// What a transfer in progress should do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Continue {
    /// Read the next chunk.
    Now,
    /// Hold, and ask again shortly. The decoder is far enough ahead that the
    /// radio can be left alone for a moment.
    Wait,
    /// Stop. Nobody wants these bytes any more.
    Abandon,
}

/// Folds the two reasons a transfer might not read right now into one answer.
///
/// The order is the whole content of this function. Abandonment is checked
/// before the throttle, because a transfer that has been abandoned *while*
/// the throttle is holding it closed would otherwise sit waiting for a lead it
/// will never be allowed to build — a download that cannot end. The bug this
/// replaces was the same shape from the other side: nothing checked
/// abandonment at all, so lifting a figure left the radio pulling a whole
/// story it had already been told nobody wanted.
pub fn next_step(abandoned: bool, may_read: bool) -> Continue {
    if abandoned {
        Continue::Abandon
    } else if may_read {
        Continue::Now
    } else {
        Continue::Wait
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Throttle {
    fetching: bool,
}

impl Throttle {
    /// Starts paused, which matters: a download that begins while a story is
    /// already playing should build its lead deliberately rather than open the
    /// throttle for the first frame and glitch it.
    pub const fn new() -> Self {
        Self { fetching: false }
    }

    pub const fn fetching(&self) -> bool {
        self.fetching
    }

    /// Decides whether to read now.
    ///
    /// `lead` is how far the download is ahead of the decoder, in whole pages.
    /// `resume_below` and `pause_above` are the two thresholds; passing them in
    /// keeps the numbers with the caller that measured them rather than buried
    /// here.
    ///
    /// A `pause_above` at or below `resume_below` would leave no gap for the
    /// previous answer to stand in, so it is treated as the caller meaning "no
    /// hysteresis" and the resume threshold decides alone.
    pub fn update(
        &mut self,
        playing: bool,
        lead: Pages,
        resume_below: Pages,
        pause_above: Pages,
    ) -> bool {
        self.fetching = if !playing {
            // Nothing to protect. The sooner this finishes the sooner the box
            // stops competing with itself at all.
            true
        } else if lead < resume_below {
            true
        } else if lead >= pause_above {
            false
        } else {
            self.fetching
        };
        self.fetching
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The precedence that matters, and the one a bug hides in: a transfer
    /// that has been abandoned must stop even while the throttle is holding it
    /// closed. Checking the throttle first would leave an abandoned download
    /// waiting for a lead it will never be allowed to build, which is a
    /// download that never ends.
    #[test]
    fn an_abandoned_transfer_stops_even_while_the_throttle_holds_it() {
        assert_eq!(next_step(true, false), Continue::Abandon);
    }

    #[test]
    fn an_abandoned_transfer_stops_rather_than_reading_on() {
        assert_eq!(next_step(true, true), Continue::Abandon);
    }

    #[test]
    fn a_throttled_transfer_waits() {
        assert_eq!(next_step(false, false), Continue::Wait);
    }

    #[test]
    fn an_open_throttle_reads_now() {
        assert_eq!(next_step(false, true), Continue::Now);
    }

    const RESUME: Pages = Pages(8);
    const PAUSE: Pages = Pages(32);

    #[test]
    fn nothing_playing_means_download_flat_out() {
        let mut throttle = Throttle::new();
        assert!(throttle.update(false, Pages(0), RESUME, PAUSE));
        assert!(throttle.update(false, Pages(1000), RESUME, PAUSE));
    }

    /// The case the whole thing exists for: a story is playing and the decoder
    /// is close behind, so the download has to run whatever it costs in
    /// glitches — running out of lead stops the audio entirely.
    #[test]
    fn a_thin_lead_wins_over_a_quiet_radio() {
        let mut throttle = Throttle::new();
        assert!(throttle.update(true, Pages(7), RESUME, PAUSE));
    }

    #[test]
    fn a_comfortable_lead_pauses_the_download() {
        let mut throttle = Throttle::new();
        throttle.update(true, Pages(0), RESUME, PAUSE);
        assert!(throttle.fetching(), "should have started");
        assert!(!throttle.update(true, Pages(32), RESUME, PAUSE));
    }

    /// The reason there are two thresholds. With one, the download stops and
    /// starts on every page the decoder consumes, the Wi-Fi never goes quiet,
    /// and the lead never grows — the worst of both.
    #[test]
    fn between_the_thresholds_the_last_decision_stands() {
        let mut throttle = Throttle::new();

        // Fell behind, so it is fetching; a middling lead does not stop it.
        throttle.update(true, Pages(2), RESUME, PAUSE);
        assert!(
            throttle.update(true, Pages(20), RESUME, PAUSE),
            "stopped too early"
        );

        // Got comfortable, so it paused; the same middling lead does not
        // restart it.
        throttle.update(true, Pages(40), RESUME, PAUSE);
        assert!(
            !throttle.update(true, Pages(20), RESUME, PAUSE),
            "restarted too early"
        );
    }

    /// Thresholds that leave no gap must not make the answer depend on history
    /// in a way the caller cannot predict.
    #[test]
    fn thresholds_with_no_gap_fall_back_to_one_decision() {
        let mut throttle = Throttle::new();
        let both = Pages(10);
        assert!(throttle.update(true, Pages(9), both, both));
        assert!(!throttle.update(true, Pages(10), both, both));
        assert!(throttle.update(true, Pages(9), both, both));
    }

    /// Starting paused is deliberate: a download begun while a story is already
    /// playing should not open the throttle for its very first frame.
    #[test]
    fn a_new_throttle_is_paused() {
        assert!(!Throttle::new().fetching());
    }
}
