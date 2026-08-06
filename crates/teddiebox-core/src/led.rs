//! Maps machine state onto the single RGB indicator.
//!
//! One LED must express playback, network and power state at once, so the
//! ordering here is a priority decision, not a lookup: the user needs to see
//! the condition that requires action.

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
    if charging {
        return LedState::Charging;
    }
    if battery == BatteryLevel::Critical {
        return LedState::LowBattery;
    }
    match playback {
        PlaybackKind::Failed => LedState::Error,
        PlaybackKind::Fetching => LedState::Fetching,
        PlaybackKind::Playing => LedState::Playing,
        PlaybackKind::Paused | PlaybackKind::Idle => LedState::Ready,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn a_critical_pack_outranks_everything_except_charging() {
        assert_eq!(
            led_for(PlaybackKind::Playing, BatteryLevel::Critical, false),
            LedState::LowBattery
        );
        assert_eq!(
            led_for(PlaybackKind::Failed, BatteryLevel::Critical, false),
            LedState::LowBattery
        );
    }

    #[test]
    fn charging_is_shown_even_on_an_empty_pack() {
        assert_eq!(
            led_for(PlaybackKind::Idle, BatteryLevel::Critical, true),
            LedState::Charging
        );
    }

    #[test]
    fn a_low_but_not_critical_pack_does_not_mask_playback() {
        assert_eq!(
            led_for(PlaybackKind::Playing, BatteryLevel::Low, false),
            LedState::Playing
        );
    }
}
