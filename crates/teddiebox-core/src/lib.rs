#![no_std]

mod battery;
pub mod board;
pub mod checksum;
pub mod console;
pub mod cushion;
mod gesture;
pub mod i2c;
pub mod input;
mod led;
pub mod pipe;
pub mod plate;
mod playback;
pub mod position;
pub mod power;
pub mod sounds;
pub mod tone;
mod types;
mod volume;
pub mod walk;
pub mod wav;

pub use battery::{BatteryConfig, BatteryLevel, BatteryModel};
pub use gesture::{Gesture, GestureConfig, GestureDetector};
pub use led::{colour_for, led_for, PlaybackKind};
pub use playback::{ContentIndex, Playback, Unavailable};
pub use types::*;
pub use volume::{db_for, VolumeModel};

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
    /// Content for this tag could not be obtained, and why.
    ContentMissing(TagUid, Unavailable),
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
    /// Stop a download nobody is waiting for any more.
    AbortFetch,
    PlayPrompt(Prompt),
    PowerOff,
}

#[derive(Debug, Clone, Copy)]
pub struct CoreConfig {
    pub volume_limit: u8,
    pub gesture: GestureConfig,
    pub battery: BatteryConfig,
    /// Milliseconds an ear must be held to count as a long press rather than
    /// a tap.
    pub long_press_ms: Millis,
    /// Milliseconds of inactivity after which the box powers itself off.
    pub idle_timeout_ms: Millis,
}

impl Default for CoreConfig {
    fn default() -> Self {
        Self {
            volume_limit: MAX_VOLUME,
            gesture: GestureConfig::default(),
            battery: BatteryConfig::default(),
            long_press_ms: 600,
            idle_timeout_ms: 5 * 60 * 1_000,
        }
    }
}

#[derive(Debug)]
pub struct Core {
    config: CoreConfig,
    volume: VolumeModel,
    gestures: GestureDetector,
    battery: BatteryModel,
    playback: Playback,
    charging: bool,
    ear_down: [Option<Millis>; 2],
    led: LedState,
    last_activity: Millis,
    last_tick: Millis,
}

impl Core {
    pub fn new(config: CoreConfig) -> Self {
        Self {
            config,
            volume: VolumeModel::new(config.volume_limit),
            gestures: GestureDetector::new(config.gesture),
            battery: BatteryModel::new(config.battery),
            playback: Playback::new(),
            charging: false,
            ear_down: [None, None],
            led: LedState::Booting,
            last_activity: 0,
            last_tick: 0,
        }
    }

    pub fn volume(&self) -> Volume {
        self.volume.current()
    }

    /// Records how far the story has got, so lifting the figure can save it.
    ///
    /// Not an `Event`, because it carries no decision and would be the most
    /// frequent one by far: the media task calls this once per decoded frame,
    /// beside the plate and the ears it already services there. It touches RAM
    /// only — the card's copy is written at chapter boundaries by whoever owns
    /// the card, which is not this.
    pub fn note_position(&mut self, pos: Position) {
        self.playback.note_position(pos);
    }

    pub fn handle<I: ContentIndex>(&mut self, event: Event, index: &I) -> Actions {
        let mut actions = Actions::new();

        // Timestamped events date themselves; untimestamped ones are dated by
        // the most recent tick, which is the only clock the core ever sees.
        // Battery and charger events deliberately do not count as activity: a
        // box sitting on the shelf still samples its pack, and treating that
        // as use would keep it awake forever.
        match event {
            Event::Tick(now) => self.last_tick = now,
            Event::EarDown(_, at) | Event::EarUp(_, at) | Event::Motion { at, .. } => {
                self.last_activity = at;
            }
            Event::TagPresent(_)
            | Event::TagAbsent
            | Event::ContentReady(_)
            | Event::ContentMissing(..) => {
                self.last_activity = self.last_tick;
            }
            Event::Battery { .. } | Event::Charger(_) | Event::TrackFinished => {}
        }

        match event {
            Event::Tick(now) => {
                let idle = now.saturating_sub(self.last_activity);
                if self.playback.kind() != PlaybackKind::Playing
                    && idle >= self.config.idle_timeout_ms
                {
                    let _ = actions.push(Action::PowerOff);
                }
            }

            Event::EarDown(ear, at) => {
                self.ear_down[ear as usize] = Some(at);
            }

            Event::EarUp(ear, at) => {
                let Some(down_at) = self.ear_down[ear as usize].take() else {
                    return actions;
                };
                let held = at.saturating_sub(down_at);
                if held >= self.config.long_press_ms {
                    let _ = actions.push(match ear {
                        Ear::Larger => Action::NextTrack,
                        Ear::Smaller => Action::PrevTrack,
                    });
                } else {
                    let changed = match ear {
                        Ear::Larger => self.volume.up(),
                        Ear::Smaller => self.volume.down(),
                    };
                    match changed {
                        Some(v) => {
                            let _ = actions.push(Action::SetVolume(v));
                        }
                        None => {
                            let _ = actions.push(Action::PlayPrompt(Prompt::VolumeLimit));
                        }
                    }
                }
            }

            Event::TagPresent(tag) => {
                let a = self.playback.on_tag_present(tag, index);
                for act in a {
                    let _ = actions.push(act);
                }
            }

            Event::TagAbsent => {
                let a = self.playback.on_tag_absent();
                for act in a {
                    let _ = actions.push(act);
                }
            }

            Event::ContentReady(tag) => {
                let a = self.playback.on_content_ready(tag, index);
                for act in a {
                    let _ = actions.push(act);
                }
            }

            Event::ContentMissing(tag, why) => {
                let a = self.playback.on_content_missing(tag, why);
                for act in a {
                    let _ = actions.push(act);
                }
            }

            Event::Motion { x, y, z, at } => {
                if let Some(g) = self.gestures.feed(x, y, z, at) {
                    let _ = actions.push(match g {
                        Gesture::Slap(Side::Right) => Action::NextTrack,
                        Gesture::Slap(Side::Left) => Action::PrevTrack,
                        Gesture::Tilt(dir) => Action::Seek(dir),
                        Gesture::TiltEnded => Action::SeekEnd,
                    });
                }
            }

            Event::Battery {
                pack_mv,
                under_load,
            } => {
                if self.battery.update(pack_mv, under_load).is_some()
                    && self.battery.level() == BatteryLevel::Low
                {
                    let _ = actions.push(Action::PlayPrompt(Prompt::BatteryLow));
                }
                if self.battery.must_shut_down() {
                    let _ = actions.push(Action::PowerOff);
                }
            }

            Event::Charger(on) => {
                self.charging = on;
            }

            Event::TrackFinished => {
                let _ = actions.push(Action::NextTrack);
            }
        }

        self.refresh_led(&mut actions);
        actions
    }

    /// Appends a `SetLed` action when the indicator should change.
    fn refresh_led(&mut self, actions: &mut Actions) {
        let want = led_for(self.playback.kind(), self.battery.level(), self.charging);
        if want != self.led {
            self.led = want;
            let _ = actions.push(Action::SetLed(want));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TAG: TagUid = TagUid([1, 2, 3, 4, 5, 6, 7, 8]);

    struct Index;
    impl ContentIndex for Index {
        fn is_available(&self, _tag: TagUid) -> bool {
            true
        }
        fn saved_position(&self, _tag: TagUid) -> Position {
            Position::Exact { page: 1 }
        }
    }

    fn core() -> Core {
        Core::new(CoreConfig::default())
    }

    fn contains(actions: &Actions, wanted: Action) -> bool {
        actions.contains(&wanted)
    }

    /// `Playback::note_position` has existed since M2 and is reachable only
    /// from its own tests, so lifting a figure has always saved the position
    /// the story *started* at. The firmware holds a `Core`, not a `Playback`.
    #[test]
    fn a_lifted_figure_saves_where_the_story_had_reached() {
        let mut c = core();
        c.handle(Event::TagPresent(TAG), &Index);

        c.note_position(Position::Exact { page: 412 });
        let actions = c.handle(Event::TagAbsent, &Index);

        assert!(contains(
            &actions,
            Action::SavePosition {
                tag: TAG,
                pos: Position::Exact { page: 412 }
            }
        ));
    }

    /// The bigger ear does the bigger thing. A stock box puts volume up and
    /// next-track on the large ear, and a child reaches for it without being
    /// told which is which.
    #[test]
    fn a_tap_on_the_larger_ear_raises_the_volume() {
        let mut c = core();
        let before = c.volume();
        c.handle(Event::EarDown(Ear::Larger, 0), &Index);
        let actions = c.handle(Event::EarUp(Ear::Larger, 100), &Index);
        assert!(c.volume() > before);
        assert!(contains(&actions, Action::SetVolume(c.volume())));
    }

    #[test]
    fn a_tap_on_the_smaller_ear_lowers_the_volume() {
        let mut c = core();
        let before = c.volume();
        c.handle(Event::EarDown(Ear::Smaller, 0), &Index);
        c.handle(Event::EarUp(Ear::Smaller, 100), &Index);
        assert!(c.volume() < before);
    }

    #[test]
    fn a_hold_on_the_larger_ear_goes_forward_and_the_smaller_goes_back() {
        let mut c = core();
        c.handle(Event::EarDown(Ear::Larger, 0), &Index);
        let forward = c.handle(Event::EarUp(Ear::Larger, 1_000), &Index);
        assert!(contains(&forward, Action::NextTrack));

        c.handle(Event::EarDown(Ear::Smaller, 2_000), &Index);
        let back = c.handle(Event::EarUp(Ear::Smaller, 3_000), &Index);
        assert!(contains(&back, Action::PrevTrack));
    }

    #[test]
    fn a_tap_at_the_volume_ceiling_prompts_instead_of_changing_volume() {
        let mut c = core();
        for i in 0..10 {
            c.handle(Event::EarDown(Ear::Larger, i * 200), &Index);
            c.handle(Event::EarUp(Ear::Larger, i * 200 + 100), &Index);
        }
        c.handle(Event::EarDown(Ear::Larger, 5_000), &Index);
        let actions = c.handle(Event::EarUp(Ear::Larger, 5_100), &Index);
        assert!(contains(&actions, Action::PlayPrompt(Prompt::VolumeLimit)));
    }

    #[test]
    fn a_long_press_on_the_right_ear_skips_forward() {
        let mut c = core();
        c.handle(Event::EarDown(Ear::Larger, 0), &Index);
        let actions = c.handle(Event::EarUp(Ear::Larger, 900), &Index);
        assert!(contains(&actions, Action::NextTrack));
        assert!(!contains(&actions, Action::SetVolume(c.volume())));
    }

    #[test]
    fn a_long_press_on_the_left_ear_skips_backward() {
        let mut c = core();
        c.handle(Event::EarDown(Ear::Smaller, 0), &Index);
        let actions = c.handle(Event::EarUp(Ear::Smaller, 900), &Index);
        assert!(contains(&actions, Action::PrevTrack));
    }

    #[test]
    fn a_release_without_a_press_is_ignored() {
        let mut c = core();
        assert!(c.handle(Event::EarUp(Ear::Larger, 100), &Index).is_empty());
    }

    #[test]
    fn a_slap_on_the_right_skips_to_the_next_track() {
        let mut c = core();
        c.handle(
            Event::Motion {
                x: 0,
                y: 0,
                z: 1000,
                at: 0,
            },
            &Index,
        );
        let actions = c.handle(
            Event::Motion {
                x: 900,
                y: 0,
                z: 1000,
                at: 20,
            },
            &Index,
        );
        assert!(contains(&actions, Action::NextTrack));
    }

    #[test]
    fn placing_a_figure_starts_playback_and_updates_the_indicator() {
        let mut c = core();
        let actions = c.handle(Event::TagPresent(TAG), &Index);
        assert!(contains(
            &actions,
            Action::Play {
                tag: TAG,
                from: Position::Exact { page: 1 }
            }
        ));
        assert!(contains(&actions, Action::SetLed(LedState::Playing)));
    }

    #[test]
    fn the_box_powers_off_after_a_long_idle() {
        let mut c = core();
        let actions = c.handle(Event::Tick(5 * 60 * 1_000 + 1), &Index);
        assert!(contains(&actions, Action::PowerOff));
    }

    #[test]
    fn the_box_does_not_power_off_while_playing() {
        let mut c = core();
        c.handle(Event::TagPresent(TAG), &Index);
        let actions = c.handle(Event::Tick(5 * 60 * 1_000 + 1), &Index);
        assert!(!contains(&actions, Action::PowerOff));
    }

    #[test]
    fn activity_defers_the_power_off() {
        let mut c = core();
        c.handle(Event::EarDown(Ear::Larger, 4 * 60 * 1_000), &Index);
        c.handle(Event::EarUp(Ear::Larger, 4 * 60 * 1_000 + 100), &Index);
        let actions = c.handle(Event::Tick(5 * 60 * 1_000 + 1), &Index);
        assert!(!contains(&actions, Action::PowerOff));
    }

    #[test]
    fn returning_the_box_to_level_ends_the_seek() {
        let mut c = core();
        c.handle(
            Event::Motion {
                x: 0,
                y: 600,
                z: 800,
                at: 0,
            },
            &Index,
        );
        c.handle(
            Event::Motion {
                x: 0,
                y: 600,
                z: 800,
                at: 400,
            },
            &Index,
        );
        let actions = c.handle(
            Event::Motion {
                x: 0,
                y: 0,
                z: 1000,
                at: 600,
            },
            &Index,
        );
        assert!(contains(&actions, Action::SeekEnd));
    }

    #[test]
    fn the_indicator_is_not_reset_when_nothing_changed() {
        let mut c = core();
        c.handle(Event::TagPresent(TAG), &Index);
        let actions = c.handle(Event::Tick(1_000), &Index);
        assert!(!actions.iter().any(|a| matches!(a, Action::SetLed(_))));
    }
}
