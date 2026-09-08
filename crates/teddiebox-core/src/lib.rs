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
pub mod place;
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
    /// The story reached its end on its own.
    ///
    /// Fed by whoever owns playback, which is the only thing that can know.
    PlaybackEnded,
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
    /// Set once the shutdown prompt has been played for the current
    /// `must_shut_down` latch, so the reducer says it once rather than on
    /// every subsequent battery reading for as long as the box has power to
    /// keep asking.
    announced_shutdown: bool,
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
            announced_shutdown: false,
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
            | Event::ContentMissing(..)
            | Event::PlaybackEnded => {
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
                // The warning follows the bucket the level settles into: Low
                // or Critical both mean "getting low", and warning on both
                // catches a fast drop that skips the Low bucket entirely.
                if let Some(BatteryLevel::Low | BatteryLevel::Critical) =
                    self.battery.update(pack_mv, under_load)
                {
                    let _ = actions.push(Action::PlayPrompt(Prompt::BatteryLow));
                }

                // The stop follows the hard cutoff, not the Critical bucket:
                // `must_shut_down` latches by design (a pack that recovers
                // voltage once the load is off is still empty), so this fires
                // once, on the false-to-true transition, rather than on every
                // reading for as long as the box has power to keep asking.
                if self.battery.must_shut_down() && !self.announced_shutdown {
                    self.announced_shutdown = true;
                    let _ = actions.push(Action::PlayPrompt(Prompt::BatteryCritical));
                    let _ = actions.push(Action::PowerOff);
                }
            }

            Event::Charger(on) => {
                self.charging = on;
            }

            Event::TrackFinished => {
                let _ = actions.push(Action::NextTrack);
            }

            Event::PlaybackEnded => {
                let a = self.playback.on_playback_ended();
                for act in a {
                    let _ = actions.push(act);
                }
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

    /// The case finding 4 showed was unreachable: a child wanders off and leaves
    /// the figure on the plate. Before `PlaybackEnded` the reducer still believed
    /// it was playing, so the timeout — gated on not-playing — never fired.
    #[test]
    fn a_finished_story_lets_the_idle_timeout_fire_with_the_figure_still_on() {
        let mut c = Core::new(CoreConfig::default());
        c.handle(Event::Tick(0), &Index);
        c.handle(Event::TagPresent(TagUid([1, 2, 3, 4, 5, 6, 7, 8])), &Index);
        c.handle(Event::PlaybackEnded, &Index);

        let actions = c.handle(Event::Tick(5 * 60 * 1_000 + 1), &Index);
        assert!(contains(&actions, Action::PowerOff));
    }

    /// A story that ran for longer than the idle timeout must not switch the box
    /// off the moment it ends: the countdown starts when the box becomes idle, not
    /// when the figure was placed. Feeding the tick only at the top of the media
    /// loop, which is inside the playback call for the whole story, is what made
    /// this a real hazard rather than a theoretical one.
    #[test]
    fn the_idle_countdown_starts_when_the_story_ends_not_when_it_began() {
        let mut c = core();
        c.handle(Event::Tick(0), &Index);
        c.handle(Event::TagPresent(TAG), &Index);
        // A long story, with the clock kept fresh throughout.
        c.handle(Event::Tick(30 * 60 * 1_000), &Index);
        c.handle(Event::PlaybackEnded, &Index);

        let actions = c.handle(Event::Tick(30 * 60 * 1_000 + 1_000), &Index);
        assert!(
            !contains(&actions, Action::PowerOff),
            "one second after the end is not idle"
        );

        let actions = c.handle(Event::Tick(30 * 60 * 1_000 + 5 * 60 * 1_000 + 1), &Index);
        assert!(
            contains(&actions, Action::PowerOff),
            "five minutes after the end is"
        );
    }

    #[test]
    fn activity_defers_the_power_off() {
        let mut c = core();
        c.handle(Event::EarDown(Ear::Larger, 4 * 60 * 1_000), &Index);
        c.handle(Event::EarUp(Ear::Larger, 4 * 60 * 1_000 + 100), &Index);
        let actions = c.handle(Event::Tick(5 * 60 * 1_000 + 1), &Index);
        assert!(!contains(&actions, Action::PowerOff));
    }

    /// The box says what it is about to do, then does it. Both exactly once: a
    /// latching shutdown that re-pushed `PowerOff` on every subsequent reading
    /// would have the box announcing its own death every two seconds.
    #[test]
    fn a_pack_reaching_critical_announces_it_and_stops_once() {
        let mut c = core();
        let mut seen = 0;
        for _ in 0..4 {
            let actions = c.handle(
                Event::Battery {
                    pack_mv: 2_900,
                    under_load: false,
                },
                &Index,
            );
            if contains(&actions, Action::PowerOff) {
                seen += 1;
                assert!(
                    contains(&actions, Action::PlayPrompt(Prompt::BatteryCritical)),
                    "it should say so in the same breath"
                );
            }
        }
        assert_eq!(seen, 1, "announced and acted on once, not per reading");

        let actions = c.handle(
            Event::Battery {
                pack_mv: 2_900,
                under_load: false,
            },
            &Index,
        );
        assert!(!contains(&actions, Action::PowerOff), "and not again after");
    }

    /// 3_250 mV is above `low_mv` (3200), so it settles into the Low bucket
    /// rather than Critical.
    #[test]
    fn a_pack_falling_to_low_warns_once() {
        let mut c = core();
        let mut seen = 0;
        for _ in 0..8 {
            let actions = c.handle(
                Event::Battery {
                    pack_mv: 3_250,
                    under_load: false,
                },
                &Index,
            );
            if contains(&actions, Action::PlayPrompt(Prompt::BatteryLow)) {
                seen += 1;
            }
        }
        assert_eq!(seen, 1);
    }

    /// The Critical bucket starts at `low_mv` (3200) and the hard cutoff is at
    /// 3000. Between them the pack is very low and the box still works, so it
    /// warns — and must not announce a shutdown it is not performing.
    #[test]
    fn the_critical_bucket_above_the_cutoff_warns_without_stopping() {
        let mut c = core();
        let mut actions_seen = Actions::new();
        for _ in 0..6 {
            for a in c.handle(
                Event::Battery {
                    pack_mv: 3_100,
                    under_load: false,
                },
                &Index,
            ) {
                let _ = actions_seen.push(a);
            }
        }
        assert!(
            contains(&actions_seen, Action::PlayPrompt(Prompt::BatteryLow)),
            "3100 mV is low enough to warn about"
        );
        assert!(
            !contains(&actions_seen, Action::PowerOff),
            "but 3100 mV is above the 3000 mV cutoff, so nothing stops"
        );
        assert!(
            !contains(&actions_seen, Action::PlayPrompt(Prompt::BatteryCritical)),
            "and announcing a shutdown that is not happening would be a lie"
        );
    }

    /// A pack that keeps falling is worth mentioning again: settling into Low
    /// warns once, and settling into Critical afterwards — a real change of
    /// bucket, not a repeat of the same reading — warns again.
    #[test]
    fn a_pack_that_falls_further_is_warned_about_again() {
        let mut c = core();
        let mut warnings = 0;
        for _ in 0..4 {
            for a in c.handle(
                Event::Battery {
                    pack_mv: 3_250,
                    under_load: false,
                },
                &Index,
            ) {
                if a == Action::PlayPrompt(Prompt::BatteryLow) {
                    warnings += 1;
                }
            }
        }
        assert_eq!(warnings, 1, "settling into Low warns once");

        for _ in 0..4 {
            for a in c.handle(
                Event::Battery {
                    pack_mv: 3_100,
                    under_load: false,
                },
                &Index,
            ) {
                if a == Action::PlayPrompt(Prompt::BatteryLow) {
                    warnings += 1;
                }
            }
        }
        assert_eq!(warnings, 2, "falling on into Critical warns again");
    }

    /// A pack recovering above a threshold is good news, and good news does
    /// not interrupt a story — but it must re-arm the warning, so a pack that
    /// falls low again after a recharge is still worth mentioning.
    #[test]
    fn recovering_is_silent_but_arms_the_warning_again() {
        let mut c = core();
        let mut warnings = 0;
        for _ in 0..4 {
            for a in c.handle(
                Event::Battery {
                    pack_mv: 3_250,
                    under_load: false,
                },
                &Index,
            ) {
                if a == Action::PlayPrompt(Prompt::BatteryLow) {
                    warnings += 1;
                }
            }
        }
        assert_eq!(warnings, 1);

        // 3_700 clears the Ok threshold plus hysteresis, so the level climbs
        // back up silently: recovery is not worth interrupting anyone for.
        for _ in 0..4 {
            for a in c.handle(
                Event::Battery {
                    pack_mv: 3_700,
                    under_load: false,
                },
                &Index,
            ) {
                assert_ne!(
                    a,
                    Action::PlayPrompt(Prompt::BatteryLow),
                    "recovering must not warn"
                );
            }
        }

        for _ in 0..4 {
            for a in c.handle(
                Event::Battery {
                    pack_mv: 3_250,
                    under_load: false,
                },
                &Index,
            ) {
                if a == Action::PlayPrompt(Prompt::BatteryLow) {
                    warnings += 1;
                }
            }
        }
        assert_eq!(warnings, 2, "falling low again after recovery warns again");
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
