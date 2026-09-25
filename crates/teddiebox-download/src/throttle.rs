//! When the download should pause to give the audio room.
//!
//! The box downloads at about 42 KB/s, and a Tonie's audio plays at about
//! 8 KB/s, so a download is five times faster than playback. Receiving over
//! Wi-Fi delays the media task, and then the audio DMA can run out and
//! restart, which is an audible glitch.
//!
//! So while a story plays, the download only runs when it is not far enough
//! ahead of the decoder, and pauses once it is. When nothing plays, it runs
//! at full speed.
//!
//! **Two thresholds (hysteresis) are essential.** With one, the download
//! would start and stop on every page. With two, it downloads in bursts and
//! is quiet in between.

use crate::units::Pages;

/// What a transfer in progress should do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Continue {
    /// Read the next chunk.
    Now,
    /// Pause, and ask again shortly. The download is far enough ahead of the
    /// decoder.
    Wait,
    /// Stop. The download is no longer needed.
    Abandon,
}

/// Combines the two reasons a transfer might not read now into one answer.
///
/// Abandonment is checked first. Otherwise a transfer abandoned while the
/// throttle is paused would wait forever.
pub fn next_step(abandoned: bool, may_read: bool) -> Continue {
    if abandoned {
        Continue::Abandon
    } else if may_read {
        Continue::Now
    } else {
        Continue::Wait
    }
}

/// Whether the download may read from the network right now.
///
/// Remembers its last answer: between the two thresholds, that answer stays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Throttle {
    fetching: bool,
}

impl Throttle {
    /// Starts paused, so a download that begins during a story does not
    /// cause a glitch in the first frame.
    pub const fn new() -> Self {
        Self { fetching: false }
    }

    pub const fn fetching(&self) -> bool {
        self.fetching
    }

    /// Decides whether to read now.
    ///
    /// `lead` is how far the download is ahead of the decoder, in whole pages.
    /// `resume_below` and `pause_above` are the two thresholds, chosen by the
    /// caller.
    ///
    /// If `pause_above` is not above `resume_below`, there is no hysteresis
    /// and the resume threshold decides alone.
    pub fn update(
        &mut self,
        playing: bool,
        lead: Pages,
        resume_below: Pages,
        pause_above: Pages,
    ) -> bool {
        self.fetching = if !playing {
            // Nothing is playing, so download at full speed.
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

    /// An abandoned transfer must stop even while the throttle is paused, or
    /// it would wait forever.
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

    /// A story is playing and the decoder is close behind, so the download
    /// must run even if it causes glitches; otherwise the audio would stop.
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

    /// Between the two thresholds the last decision stays, so the download
    /// does not start and stop on every page.
    #[test]
    fn between_the_thresholds_the_last_decision_stands() {
        let mut throttle = Throttle::new();

        // Fell behind, so it is downloading; a middle lead does not stop it.
        throttle.update(true, Pages(2), RESUME, PAUSE);
        assert!(
            throttle.update(true, Pages(20), RESUME, PAUSE),
            "stopped too early"
        );

        // Far enough ahead, so it paused; the same middle lead does not
        // restart it.
        throttle.update(true, Pages(40), RESUME, PAUSE);
        assert!(
            !throttle.update(true, Pages(20), RESUME, PAUSE),
            "restarted too early"
        );
    }

    /// With no gap between the thresholds, the answer does not depend on
    /// the previous one.
    #[test]
    fn thresholds_with_no_gap_fall_back_to_one_decision() {
        let mut throttle = Throttle::new();
        let both = Pages(10);
        assert!(throttle.update(true, Pages(9), both, both));
        assert!(!throttle.update(true, Pages(10), both, both));
        assert!(throttle.update(true, Pages(9), both, both));
    }

    /// Starts paused, so a download begun during a story does not glitch the
    /// first frame.
    #[test]
    fn a_new_throttle_is_paused() {
        assert!(!Throttle::new().fetching());
    }
}
