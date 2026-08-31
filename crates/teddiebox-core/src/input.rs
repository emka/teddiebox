//! The box's inputs: two ears and the wake line.
//!
//! Debouncing and polarity, decided where a host test can see them. The pins
//! themselves belong to the firmware.

/// The left ear. Active low.
pub const EAR_LEFT: u8 = 20;
/// The right ear. Active low.
pub const EAR_RIGHT: u8 = 21;
/// Wake, from the button or the charger. Documented as 1 = inactive.
pub const WAKE: u8 = 7;

/// How long a level must hold before it counts.
///
/// A starting point, not a measurement: bench step 2 exists partly to find the
/// real bounce duration of these particular switches, and this should be set
/// from what it finds rather than from taste.
pub const DEBOUNCE_MS: u32 = 20;

/// True when an ear is pressed. The ears are wired active low.
pub const fn ear_pressed(pin_is_high: bool) -> bool {
    !pin_is_high
}

/// True when the wake line is asserted. Documented as 1 = inactive.
pub const fn wake_asserted(pin_is_high: bool) -> bool {
    !pin_is_high
}

/// What a settled change was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    Pressed,
    Released,
}

/// One input, debounced.
///
/// Fed a raw level and a millisecond clock; reports an edge exactly once, when
/// a new level has held for [`DEBOUNCE_MS`].
#[derive(Debug, Clone)]
pub struct Debounced {
    settled: bool,
    candidate: bool,
    since_ms: u32,
}

impl Debounced {
    /// Starts settled in the released state, which is how the box boots.
    pub const fn released() -> Self {
        Self {
            settled: false,
            candidate: false,
            since_ms: 0,
        }
    }

    /// Feeds one sample. `Some` exactly once per settled change.
    ///
    /// `now_ms` may wrap — it does every 49 days — so the elapsed time is a
    /// wrapping subtraction rather than a difference.
    pub fn update(&mut self, pressed: bool, now_ms: u32) -> Option<Edge> {
        if pressed != self.candidate {
            self.candidate = pressed;
            self.since_ms = now_ms;
            return None;
        }

        if pressed == self.settled {
            return None;
        }

        if now_ms.wrapping_sub(self.since_ms) < DEBOUNCE_MS {
            return None;
        }

        self.settled = pressed;
        Some(if pressed {
            Edge::Pressed
        } else {
            Edge::Released
        })
    }

    pub const fn is_pressed(&self) -> bool {
        self.settled
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A press that has not settled is not a press. The ears are the box's
    /// only controls, so a bounce counted as a second press skips a chapter.
    #[test]
    fn a_bounce_shorter_than_the_window_reports_nothing() {
        let mut button = Debounced::released();
        assert_eq!(button.update(true, 0), None);
        assert_eq!(button.update(false, 5), None);
        assert_eq!(button.update(true, 10), None);
        assert_eq!(button.update(false, 15), None);
    }

    #[test]
    fn a_level_held_for_the_window_reports_one_press() {
        let mut button = Debounced::released();
        assert_eq!(button.update(true, 0), None);
        assert_eq!(button.update(true, DEBOUNCE_MS - 1), None);
        assert_eq!(button.update(true, DEBOUNCE_MS), Some(Edge::Pressed));
        assert_eq!(button.update(true, DEBOUNCE_MS + 100), None, "only once");
    }

    #[test]
    fn releasing_is_reported_too() {
        let mut button = Debounced::released();
        button.update(true, 0);
        assert_eq!(button.update(true, DEBOUNCE_MS), Some(Edge::Pressed));
        assert_eq!(button.update(false, DEBOUNCE_MS + 1), None);
        assert_eq!(
            button.update(false, DEBOUNCE_MS * 2 + 1),
            Some(Edge::Released)
        );
    }

    /// The clock wraps every 49 days. A box left on a shelf must not become
    /// unresponsive, and `now - since` is where that would happen.
    #[test]
    fn the_millisecond_clock_may_wrap() {
        let mut button = Debounced::released();
        assert_eq!(button.update(true, u32::MAX - 5), None);
        assert_eq!(
            button.update(true, (u32::MAX - 5).wrapping_add(DEBOUNCE_MS)),
            Some(Edge::Pressed)
        );
    }

    /// Both ears are wired active low, so a pressed ear reads as a low pin.
    #[test]
    fn an_ear_is_pressed_when_its_pin_reads_low() {
        assert!(ear_pressed(false));
        assert!(!ear_pressed(true));
    }

    /// The wake input documents 1 as inactive, same sense as the ears.
    #[test]
    fn wake_is_asserted_when_its_pin_reads_low() {
        assert!(wake_asserted(false));
        assert!(!wake_asserted(true));
    }
}
