//! Maps machine state onto the single RGB indicator.
//!
//! One LED must express playback, network and power state at once, so the
//! ordering here is a priority decision, not a lookup: the user needs to see
//! the condition that requires action.

use crate::board::Colour;
use crate::{BatteryLevel, LedState};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackKind {
    Idle,
    Playing,
    Paused,
    Fetching,
    Failed,
}

pub fn led_for(playback: PlaybackKind, battery: BatteryLevel, charging: bool) -> LedState {
    // A failure first, charger or no charger: it is the only state here that
    // will not resolve itself by waiting.
    if playback == PlaybackKind::Failed {
        return LedState::Error;
    }
    // A pack this close to empty outranks the story it is about to interrupt —
    // unless it is already on a charger, where the situation is being dealt
    // with and saying so is more useful than raising an alarm.
    if battery == BatteryLevel::Critical && !charging {
        return LedState::BatteryCritical;
    }
    // Above the tired pack, because a download is the one state here the child
    // can neither see nor hear: the box is silent and looks idle, and painting
    // it orange would hide the one thing worth waiting for.
    if playback == PlaybackKind::Fetching {
        return LedState::Fetching;
    }
    if battery == BatteryLevel::Low && !charging {
        return LedState::BatteryLow;
    }
    match playback {
        // Handled above; repeated here because the compiler cannot know that.
        PlaybackKind::Failed | PlaybackKind::Fetching => LedState::Error,
        PlaybackKind::Playing => LedState::Playing,
        // Charging says nothing while a story plays — that is what the green
        // is for — so it is left to the states where the box has nothing else
        // to report.
        PlaybackKind::Paused | PlaybackKind::Idle if charging => LedState::Charging,
        PlaybackKind::Paused | PlaybackKind::Idle => LedState::Ready,
    }
}

/// The colour that stands for a state.
///
/// Kept apart from [`led_for`], which decides *which* state is worth showing:
/// this only says what it looks like. Both halves are here rather than in the
/// firmware because neither needs a peripheral to be tested, and the one that
/// decides what a child sees is the last place to want an untested branch.
pub const fn colour_for(state: LedState) -> Colour {
    match state {
        // Nothing to say, and nothing lit. Standby, and the state a box holds
        // while it is being switched off.
        LedState::Off => Colour::Off,
        // On, and either about to be useful or already useful. The same green
        // for both: a story playing is the box working as intended, which is
        // not news worth its own colour.
        LedState::Booting | LedState::Ready | LedState::Playing => Colour::Green,
        LedState::Fetching => Colour::Blue,
        LedState::Charging => Colour::Cyan,
        LedState::BatteryLow => Colour::Orange,
        // The same red as a fault, deliberately: a box about to switch itself
        // off has failed to be a box, whatever the reason.
        LedState::BatteryCritical | LedState::Error => Colour::Red,
        LedState::Setup => Colour::Magenta,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::Colour;

    /// The five colours asked for, in one place, because the mapping is the
    /// whole of what anybody looking at the box can see.
    #[test]
    fn each_state_shows_the_colour_it_was_given() {
        assert_eq!(colour_for(LedState::Ready), Colour::Green);
        assert_eq!(colour_for(LedState::Playing), Colour::Green);
        assert_eq!(colour_for(LedState::Fetching), Colour::Blue);
        assert_eq!(colour_for(LedState::BatteryLow), Colour::Orange);
        assert_eq!(colour_for(LedState::BatteryCritical), Colour::Red);
        assert_eq!(colour_for(LedState::Error), Colour::Red);
        assert_eq!(colour_for(LedState::Off), Colour::Off);
    }

    /// The seam to the task that owns the LED is one atomic byte, so every
    /// state has to survive the round trip. A state that does not is a colour
    /// that silently never appears.
    #[test]
    fn every_state_survives_the_trip_through_an_atomic() {
        for state in LedState::ALL {
            assert_eq!(LedState::from_code(state.code()), Some(state));
        }
    }

    /// A byte that is not a state must not be mistaken for one — the atomic
    /// starts at zero and anything could write to it.
    #[test]
    fn a_byte_that_names_no_state_is_refused() {
        assert_eq!(LedState::from_code(200), None);
    }

    /// The pack this box carries goes flat without warning, so the warning it
    /// can give has to arrive while there is still charge to act on. `Low` is
    /// 3300 mV, `Critical` is 3000, and the fall between them is minutes.
    #[test]
    fn a_low_pack_warns_in_orange_while_a_story_plays() {
        assert_eq!(
            led_for(PlaybackKind::Playing, BatteryLevel::Low, false),
            LedState::BatteryLow
        );
    }

    /// Distinct from `BatteryLow`, and deliberately the same red as a fault:
    /// by this point the box is about to switch itself off, which is a
    /// failure of the same order as a story that cannot be played.
    #[test]
    fn a_critical_pack_is_red_rather_than_orange() {
        assert_eq!(
            led_for(PlaybackKind::Playing, BatteryLevel::Critical, false),
            LedState::BatteryCritical
        );
    }

    /// A download is the one thing here the child can neither see nor hear —
    /// the box is silent and apparently idle — so it outranks the tired pack
    /// that would otherwise paint the same idle box orange.
    #[test]
    fn fetching_outranks_a_low_pack() {
        assert_eq!(
            led_for(PlaybackKind::Fetching, BatteryLevel::Low, false),
            LedState::Fetching
        );
    }

    /// Charging used to outrank everything, which on a bench box — always on
    /// its charger — meant no other colour could ever be seen.
    #[test]
    fn charging_does_not_hide_a_download() {
        assert_eq!(
            led_for(PlaybackKind::Fetching, BatteryLevel::Ok, true),
            LedState::Fetching
        );
    }

    #[test]
    fn charging_does_not_hide_a_failure() {
        assert_eq!(
            led_for(PlaybackKind::Failed, BatteryLevel::Ok, true),
            LedState::Error
        );
    }

    /// What is left for charging to say: the box is idle and plugged in.
    #[test]
    fn charging_shows_when_there_is_nothing_else_to_show() {
        assert_eq!(
            led_for(PlaybackKind::Idle, BatteryLevel::Ok, true),
            LedState::Charging
        );
    }

    #[test]
    fn playing_on_a_healthy_pack_shows_playing() {
        assert_eq!(
            led_for(PlaybackKind::Playing, BatteryLevel::Ok, false),
            LedState::Playing
        );
    }

    #[test]
    fn idle_on_a_healthy_pack_shows_ready() {
        assert_eq!(
            led_for(PlaybackKind::Idle, BatteryLevel::Full, false),
            LedState::Ready
        );
    }

    #[test]
    fn fetching_outranks_ready() {
        assert_eq!(
            led_for(PlaybackKind::Fetching, BatteryLevel::Full, false),
            LedState::Fetching
        );
    }

    #[test]
    fn a_failure_outranks_playback_state() {
        assert_eq!(
            led_for(PlaybackKind::Failed, BatteryLevel::Full, false),
            LedState::Error
        );
    }

    /// A flat pack still outranks a playing story: the box is about to stop
    /// either way, and which of the two the LED names decides whether anybody
    /// reaches for the charger.
    #[test]
    fn a_critical_pack_outranks_playback() {
        assert_eq!(
            led_for(PlaybackKind::Playing, BatteryLevel::Critical, false),
            LedState::BatteryCritical
        );
    }

    /// On the charger and nearly empty, the charger is the more useful fact —
    /// it says the situation is already being dealt with.
    #[test]
    fn charging_answers_an_empty_pack() {
        assert_eq!(
            led_for(PlaybackKind::Idle, BatteryLevel::Critical, true),
            LedState::Charging
        );
    }

    #[test]
    fn setup_has_a_colour_of_its_own() {
        for other in LedState::ALL {
            if other != LedState::Setup {
                assert_ne!(
                    colour_for(LedState::Setup),
                    colour_for(other),
                    "setup must not be mistakable for {other:?}"
                );
            }
        }
    }
}
