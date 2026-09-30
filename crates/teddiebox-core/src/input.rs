//! The box's inputs: two ears and the wake line.
//!
//! Debouncing and pin polarity, kept here so they can be tested on the host.
//! The firmware owns the pins themselves.

/// The larger ear, on the box's right. Active low.
pub const EAR_LARGER: u8 = 20;
/// The smaller ear, on the box's left. Active low.
pub const EAR_SMALLER: u8 = 21;
/// Wake, from the button or the charger. Documented as 1 = inactive.
pub const WAKE: u8 = 7;

/// How long an ear must be held to mean "next chapter" rather than "louder".
///
/// Used by the firmware task that reads the ear pins, which sends
/// `Event::EarHeld` when a press lasts this long. The reducer does not use it.
///
/// Chosen by trying it on the box.
pub const LONG_PRESS_MS: u32 = 600;

/// How long a level must hold before it counts.
///
/// A starting value, not measured. It should be set from the measured bounce
/// time of these switches.
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
    /// `now_ms` wraps every 49 days, so elapsed time uses a wrapping
    /// subtraction.
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
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bounce shorter than the debounce time is not a press.
    #[test]
    fn a_bounce_shorter_than_the_window_reports_nothing() {
        // Given
        let mut button = Debounced::released();

        // When
        let edges = [(true, 0), (false, 5), (true, 10), (false, 15)]
            .map(|(pressed, at)| button.update(pressed, at));

        // Then
        assert_eq!(edges, [None; 4]);
    }

    #[test]
    fn a_level_held_for_the_window_reports_a_press() {
        // Given
        let mut button = Debounced::released();
        button.update(true, 0);

        // When
        let edges = [DEBOUNCE_MS - 1, DEBOUNCE_MS].map(|at| button.update(true, at));

        // Then
        assert_eq!(edges, [None, Some(Edge::Pressed)]);
    }

    #[test]
    fn a_held_press_is_reported_only_once() {
        // Given
        let mut button = Debounced::released();
        button.update(true, 0);
        assert_eq!(button.update(true, DEBOUNCE_MS), Some(Edge::Pressed));

        // When
        let edge = button.update(true, DEBOUNCE_MS + 100);

        // Then
        assert_eq!(edge, None);
    }

    #[test]
    fn releasing_is_reported_too() {
        // Given
        let mut button = Debounced::released();
        button.update(true, 0);
        assert_eq!(button.update(true, DEBOUNCE_MS), Some(Edge::Pressed));

        // When
        let edges = [DEBOUNCE_MS + 1, DEBOUNCE_MS * 2 + 1].map(|at| button.update(false, at));

        // Then
        assert_eq!(edges, [None, Some(Edge::Released)]);
    }

    /// The clock wraps every 49 days; the ears must keep working after that.
    #[test]
    fn the_millisecond_clock_may_wrap() {
        // Given
        let mut button = Debounced::released();
        let before_the_wrap = u32::MAX - 5;

        // When
        let edges = [before_the_wrap, before_the_wrap.wrapping_add(DEBOUNCE_MS)]
            .map(|at| button.update(true, at));

        // Then
        assert_eq!(edges, [None, Some(Edge::Pressed)]);
    }

    /// Both ears are wired active low, so a pressed ear reads as a low pin.
    #[test]
    fn an_ear_is_pressed_when_its_pin_reads_low() {
        // Given
        let (low, high) = (false, true);

        // When
        let pressed = [low, high].map(ear_pressed);

        // Then
        assert_eq!(pressed, [true, false]);
    }

    /// The wake input documents 1 as inactive, same sense as the ears.
    #[test]
    fn wake_is_asserted_when_its_pin_reads_low() {
        // Given
        let (low, high) = (false, true);

        // When
        let asserted = [low, high].map(wake_asserted);

        // Then
        assert_eq!(asserted, [true, false]);
    }
}
