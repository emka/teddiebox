//! Slap and tilt recognition from raw accelerometer samples.
//!
//! **Every threshold in `GestureConfig` is uncalibrated.** The defaults are
//! plausible starting points, not measurements. Phase B step 5 captures real
//! traces from the box and the values are fitted then. Until that happens,
//! these tests prove the algorithm's shape, not its sensitivity.

use crate::{Millis, SeekDir, Side};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gesture {
    /// A sharp transient on the X axis: skip a track.
    Slap(Side),
    /// A sustained pitch away from level: seek within the track.
    Tilt(SeekDir),
    /// The box returned to level; stop seeking.
    TiltEnded,
}

#[derive(Debug, Clone, Copy)]
pub struct GestureConfig {
    /// Milli-g of X-axis transient that counts as a slap.
    pub slap_threshold_mg: i16,
    /// Milliseconds after a slap during which further slaps are ignored,
    /// so one physical tap cannot register twice.
    pub slap_refractory_ms: Millis,
    /// Milli-g of Y-axis deviation from level that counts as a tilt.
    pub tilt_threshold_mg: i16,
    /// Milliseconds the box must be held tilted before seeking starts,
    /// so that carrying it around does not trigger a seek.
    pub tilt_hold_ms: Millis,
}

impl Default for GestureConfig {
    fn default() -> Self {
        Self {
            slap_threshold_mg: 700,
            slap_refractory_ms: 400,
            tilt_threshold_mg: 500,
            tilt_hold_ms: 300,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct GestureDetector {
    config: GestureConfig,
    last_slap: Option<Millis>,
    tilt_since: Option<(SeekDir, Millis)>,
    tilt_reported: bool,
}

impl GestureDetector {
    pub fn new(config: GestureConfig) -> Self {
        Self {
            config,
            last_slap: None,
            tilt_since: None,
            tilt_reported: false,
        }
    }

    pub fn feed(&mut self, x: i16, y: i16, z: i16, at: Millis) -> Option<Gesture> {
        let _ = z;

        // `unsigned_abs`, not `abs`: the sensor can report i16::MIN, which has
        // no positive counterpart and would overflow.
        if x.unsigned_abs() >= self.config.slap_threshold_mg.unsigned_abs() {
            let ready = match self.last_slap {
                None => true,
                Some(t) => at.saturating_sub(t) >= self.config.slap_refractory_ms,
            };
            if ready {
                self.last_slap = Some(at);
                return Some(Gesture::Slap(if x > 0 { Side::Right } else { Side::Left }));
            }
        }

        let tilted = if y >= self.config.tilt_threshold_mg {
            Some(SeekDir::Forward)
        } else if y <= -self.config.tilt_threshold_mg {
            Some(SeekDir::Backward)
        } else {
            None
        };

        match (tilted, self.tilt_since) {
            // Newly tilted, or tilted the other way: restart the hold timer.
            (Some(dir), None) => {
                self.tilt_since = Some((dir, at));
                self.tilt_reported = false;
            }
            (Some(dir), Some((held, _))) if held != dir => {
                self.tilt_since = Some((dir, at));
                self.tilt_reported = false;
            }
            // Still tilted the same way: report once the hold elapses.
            (Some(dir), Some((_, since))) => {
                if !self.tilt_reported && at.saturating_sub(since) >= self.config.tilt_hold_ms {
                    self.tilt_reported = true;
                    return Some(Gesture::Tilt(dir));
                }
            }
            // Back to level.
            (None, Some(_)) => {
                let was_reported = self.tilt_reported;
                self.tilt_since = None;
                self.tilt_reported = false;
                if was_reported {
                    return Some(Gesture::TiltEnded);
                }
            }
            (None, None) => {}
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detector() -> GestureDetector {
        GestureDetector::new(GestureConfig::default())
    }

    /// A box sitting still: one g downward on Z, nothing on X or Y.
    const LEVEL: (i16, i16, i16) = (0, 0, 1000);

    #[test]
    fn a_level_box_reports_nothing() {
        let mut d = detector();
        assert_eq!(d.feed(LEVEL.0, LEVEL.1, LEVEL.2, 0), None);
    }

    #[test]
    fn a_sharp_positive_transient_is_a_slap_on_the_right() {
        let mut d = detector();
        d.feed(LEVEL.0, LEVEL.1, LEVEL.2, 0);
        assert_eq!(d.feed(900, 0, 1000, 20), Some(Gesture::Slap(Side::Right)));
    }

    #[test]
    fn a_sharp_negative_transient_is_a_slap_on_the_left() {
        let mut d = detector();
        d.feed(LEVEL.0, LEVEL.1, LEVEL.2, 0);
        assert_eq!(d.feed(-900, 0, 1000, 20), Some(Gesture::Slap(Side::Left)));
    }

    #[test]
    fn a_gentle_push_is_not_a_slap() {
        let mut d = detector();
        d.feed(LEVEL.0, LEVEL.1, LEVEL.2, 0);
        assert_eq!(d.feed(300, 0, 1000, 20), None);
    }

    #[test]
    fn one_tap_cannot_register_twice() {
        let mut d = detector();
        d.feed(LEVEL.0, LEVEL.1, LEVEL.2, 0);
        assert!(d.feed(900, 0, 1000, 20).is_some());
        assert_eq!(
            d.feed(900, 0, 1000, 100),
            None,
            "still inside the refractory window"
        );
        assert!(d.feed(900, 0, 1000, 500).is_some(), "window has expired");
    }

    #[test]
    fn the_most_negative_sample_the_sensor_can_report_is_still_a_slap() {
        let mut d = detector();
        assert_eq!(
            d.feed(i16::MIN, 0, 1000, 20),
            Some(Gesture::Slap(Side::Left)),
            "negating i16::MIN must not overflow"
        );
    }

    #[test]
    fn a_brief_tilt_does_not_seek() {
        let mut d = detector();
        d.feed(0, 600, 800, 0);
        assert_eq!(
            d.feed(0, 600, 800, 100),
            None,
            "not held long enough to be deliberate"
        );
    }

    #[test]
    fn a_held_forward_tilt_seeks_forward() {
        let mut d = detector();
        d.feed(0, 600, 800, 0);
        assert_eq!(
            d.feed(0, 600, 800, 400),
            Some(Gesture::Tilt(SeekDir::Forward))
        );
    }

    #[test]
    fn a_held_backward_tilt_seeks_backward() {
        let mut d = detector();
        d.feed(0, -600, 800, 0);
        assert_eq!(
            d.feed(0, -600, 800, 400),
            Some(Gesture::Tilt(SeekDir::Backward))
        );
    }

    #[test]
    fn a_sustained_tilt_is_reported_once_not_repeatedly() {
        let mut d = detector();
        d.feed(0, 600, 800, 0);
        assert!(d.feed(0, 600, 800, 400).is_some());
        assert_eq!(d.feed(0, 600, 800, 500), None);
    }

    #[test]
    fn returning_to_level_ends_the_seek() {
        let mut d = detector();
        d.feed(0, 600, 800, 0);
        d.feed(0, 600, 800, 400);
        assert_eq!(d.feed(0, 0, 1000, 600), Some(Gesture::TiltEnded));
    }

    #[test]
    fn returning_to_level_without_having_sought_reports_nothing() {
        let mut d = detector();
        d.feed(0, 600, 800, 0);
        assert_eq!(d.feed(0, 0, 1000, 100), None);
    }
}
