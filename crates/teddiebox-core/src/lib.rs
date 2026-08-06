#![no_std]

mod battery;
mod gesture;
mod led;
mod playback;
mod types;
mod volume;

pub use battery::{BatteryConfig, BatteryLevel, BatteryModel};
pub use gesture::{Gesture, GestureConfig, GestureDetector};
pub use led::{led_for, PlaybackKind};
pub use playback::{ContentIndex, Playback};
pub use types::*;
pub use volume::VolumeModel;

use heapless::Vec;

/// Upper bound on the actions one event may produce.
pub const MAX_ACTIONS: usize = 8;

pub type Actions = Vec<Action, MAX_ACTIONS>;

/// Everything that can happen to the box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// Periodic tick. Drives timeouts; the only way time advances.
    Tick(Millis),
    EarDown(Ear, Millis),
    EarUp(Ear, Millis),
    TagPresent(TagUid),
    TagAbsent,
    /// One accelerometer sample, in milli-g per axis.
    Motion {
        x: i16,
        y: i16,
        z: i16,
        at: Millis,
    },
    /// Pack voltage in millivolts, and whether the box was drawing playback
    /// current when it was sampled.
    Battery {
        pack_mv: u16,
        under_load: bool,
    },
    Charger(bool),
    /// The current track reached its end.
    TrackFinished,
    /// Content for this tag is now available locally.
    ContentReady(TagUid),
    /// Content for this tag could not be obtained.
    ContentMissing(TagUid),
}

/// Everything the firmware may be asked to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Play {
        tag: TagUid,
        from: Position,
    },
    Pause,
    Stop,
    SeekTo(Position),
    NextTrack,
    PrevTrack,
    /// Begin seeking within the current track.
    Seek(SeekDir),
    /// Stop an in-progress seek and resume normal playback.
    SeekEnd,
    SetVolume(Volume),
    SetLed(LedState),
    SavePosition {
        tag: TagUid,
        pos: Position,
    },
    RequestContent(TagUid),
    PlayPrompt(Prompt),
    PowerOff,
}

#[derive(Debug, Default)]
pub struct Core {}

impl Core {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn handle(&mut self, event: Event) -> Actions {
        let _ = event;
        Actions::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unremarkable_tick_produces_no_actions() {
        let mut core = Core::new();
        assert!(core.handle(Event::Tick(1_000)).is_empty());
    }
}
