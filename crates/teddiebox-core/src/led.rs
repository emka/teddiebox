//! Chooses what the single RGB LED shows.
//!
//! One LED has to show playback, network and battery state, so the order of
//! the checks below is a priority: show the state that most needs attention.

use crate::{BatteryLevel, LedState};
use teddiebox_board::Colour;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackKind {
    Idle,
    Playing,
    Paused,
    Fetching,
    Failed,
}

pub fn led_for(playback: PlaybackKind, battery: BatteryLevel, charging: bool) -> LedState {
    // A failure comes first, even while charging: it is the only state that
    // will not fix itself.
    if playback == PlaybackKind::Failed {
        return LedState::Error;
    }
    // A nearly empty pack comes before playback, unless it is already
    // charging.
    if battery == BatteryLevel::Critical && !charging {
        return LedState::BatteryCritical;
    }
    // Before a low pack, because during a download the box is silent and
    // looks idle; the LED is the only sign that something is happening.
    if playback == PlaybackKind::Fetching {
        return LedState::Fetching;
    }
    if battery == BatteryLevel::Low && !charging {
        return LedState::BatteryLow;
    }
    match playback {
        // Handled above; listed here so the match is complete.
        PlaybackKind::Failed | PlaybackKind::Fetching => LedState::Error,
        PlaybackKind::Playing => LedState::Playing,
        // Charging is shown only when the box has nothing else to show.
        PlaybackKind::Paused | PlaybackKind::Idle if charging => LedState::Charging,
        PlaybackKind::Paused | PlaybackKind::Idle => LedState::Ready,
    }
}

/// The colour that stands for a state.
///
/// [`led_for`] decides *which* state to show; this decides what it looks
/// like. Both live here rather than in the firmware so they can be tested on
/// the host.
pub const fn colour_for(state: LedState) -> Colour {
    match state {
        // Standby, and while the box is switching off.
        LedState::Off => Colour::Off,
        // The box is working normally.
        LedState::Booting | LedState::Ready | LedState::Playing => Colour::Green,
        LedState::Fetching => Colour::Blue,
        LedState::Charging => Colour::Cyan,
        LedState::BatteryLow => Colour::Orange,
        // The same red as a fault: either way, the box cannot play.
        LedState::BatteryCritical | LedState::Error => Colour::Red,
        LedState::Setup => Colour::Magenta,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use teddiebox_board::Colour;

    #[test]
    fn each_state_shows_the_colour_it_was_given() {
        // Given
        let states = LedState::ALL;

        // When
        let shown = states.map(|state| (state, colour_for(state)));

        // Then
        assert_eq!(
            shown,
            [
                (LedState::Off, Colour::Off),
                (LedState::Booting, Colour::Green),
                (LedState::Ready, Colour::Green),
                (LedState::Playing, Colour::Green),
                (LedState::Fetching, Colour::Blue),
                (LedState::Charging, Colour::Cyan),
                (LedState::BatteryLow, Colour::Orange),
                (LedState::BatteryCritical, Colour::Red),
                (LedState::Error, Colour::Red),
                (LedState::Setup, Colour::Magenta),
            ]
        );
    }

    /// The state reaches the LED task as one atomic byte, so every state must
    /// survive the round trip.
    #[test]
    fn every_state_survives_the_trip_through_an_atomic() {
        // Given
        let states = LedState::ALL;

        // When
        let round_tripped = states.map(|state| LedState::from_code(state.code()));

        // Then
        assert_eq!(round_tripped, states.map(Some));
    }

    /// A byte that is not a state must not be mistaken for one.
    #[test]
    fn a_byte_that_names_no_state_is_refused() {
        // Given
        let not_a_state = 200;

        // When
        let state = LedState::from_code(not_a_state);

        // Then
        assert_eq!(state, None);
    }

    /// The pack goes flat quickly at the end, so the warning must show even
    /// while a story plays.
    #[test]
    fn a_low_pack_warns_in_orange_while_a_story_plays() {
        // Given
        let (playback, battery, charging) = (PlaybackKind::Playing, BatteryLevel::Low, false);

        // When
        let state = led_for(playback, battery, charging);

        // Then
        assert_eq!(state, LedState::BatteryLow);
    }

    /// Critical is red, like a fault, because the box is about to switch off.
    #[test]
    fn a_critical_pack_is_red_rather_than_orange() {
        // Given
        let (playback, battery, charging) = (PlaybackKind::Playing, BatteryLevel::Critical, false);

        // When
        let state = led_for(playback, battery, charging);

        // Then
        assert_eq!(state, LedState::BatteryCritical);
    }

    /// During a download the box is silent and looks idle, so the download is
    /// shown instead of a low pack.
    #[test]
    fn fetching_outranks_a_low_pack() {
        // Given
        let (playback, battery, charging) = (PlaybackKind::Fetching, BatteryLevel::Low, false);

        // When
        let state = led_for(playback, battery, charging);

        // Then
        assert_eq!(state, LedState::Fetching);
    }

    /// Charging must not hide other states; a box that is always on its
    /// charger would otherwise never show anything else.
    #[test]
    fn charging_does_not_hide_a_download() {
        // Given
        let (playback, battery, charging) = (PlaybackKind::Fetching, BatteryLevel::Ok, true);

        // When
        let state = led_for(playback, battery, charging);

        // Then
        assert_eq!(state, LedState::Fetching);
    }

    #[test]
    fn charging_does_not_hide_a_failure() {
        // Given
        let (playback, battery, charging) = (PlaybackKind::Failed, BatteryLevel::Ok, true);

        // When
        let state = led_for(playback, battery, charging);

        // Then
        assert_eq!(state, LedState::Error);
    }

    /// Charging shows when the box is idle and plugged in.
    #[test]
    fn charging_shows_when_there_is_nothing_else_to_show() {
        // Given
        let (playback, battery, charging) = (PlaybackKind::Idle, BatteryLevel::Ok, true);

        // When
        let state = led_for(playback, battery, charging);

        // Then
        assert_eq!(state, LedState::Charging);
    }

    #[test]
    fn playing_on_a_healthy_pack_shows_playing() {
        // Given
        let (playback, battery, charging) = (PlaybackKind::Playing, BatteryLevel::Ok, false);

        // When
        let state = led_for(playback, battery, charging);

        // Then
        assert_eq!(state, LedState::Playing);
    }

    #[test]
    fn idle_on_a_healthy_pack_shows_ready() {
        // Given
        let (playback, battery, charging) = (PlaybackKind::Idle, BatteryLevel::Full, false);

        // When
        let state = led_for(playback, battery, charging);

        // Then
        assert_eq!(state, LedState::Ready);
    }

    #[test]
    fn fetching_outranks_ready() {
        // Given
        let (playback, battery, charging) = (PlaybackKind::Fetching, BatteryLevel::Full, false);

        // When
        let state = led_for(playback, battery, charging);

        // Then
        assert_eq!(state, LedState::Fetching);
    }

    #[test]
    fn a_failure_outranks_playback_state() {
        // Given
        let (playback, battery, charging) = (PlaybackKind::Failed, BatteryLevel::Full, false);

        // When
        let state = led_for(playback, battery, charging);

        // Then
        assert_eq!(state, LedState::Error);
    }

    /// A nearly empty pack is shown instead of playback, so somebody reaches
    /// for the charger.
    #[test]
    fn a_critical_pack_outranks_playback() {
        // Given
        let (playback, battery, charging) = (PlaybackKind::Playing, BatteryLevel::Critical, false);

        // When
        let state = led_for(playback, battery, charging);

        // Then
        assert_eq!(state, LedState::BatteryCritical);
    }

    /// Nearly empty but charging: show charging, since the problem is being
    /// fixed.
    #[test]
    fn charging_answers_an_empty_pack() {
        // Given
        let (playback, battery, charging) = (PlaybackKind::Idle, BatteryLevel::Critical, true);

        // When
        let state = led_for(playback, battery, charging);

        // Then
        assert_eq!(state, LedState::Charging);
    }

    #[test]
    fn setup_has_a_colour_of_its_own() {
        // Given
        let setup = colour_for(LedState::Setup);

        // When
        let others = LedState::ALL.map(|state| (state, colour_for(state)));

        // Then
        for (other, colour) in others {
            if other != LedState::Setup {
                assert_ne!(setup, colour, "setup must not be mistakable for {other:?}");
            }
        }
    }
}
