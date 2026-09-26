#![no_std]

mod battery;
pub mod checksum;
pub mod cue;
pub mod cushion;
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
pub use led::{colour_for, led_for, PlaybackKind};
pub use playback::{ContentIndex, Freshness, Playback, Unavailable};
pub use types::*;
pub use volume::{db_for, VolumeModel, HEADPHONE_OFFSET_DB};

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
    /// The ear has been down long enough to count as a hold, not a tap.
    ///
    /// Sent by the task that reads the ear's pin. The reducer only sees
    /// events and ticks at most once a second, so it cannot notice the moment
    /// a press becomes a hold. This lets a chapter change while the ear is
    /// still held down.
    EarHeld(Ear, Millis),
    EarUp(Ear, Millis),
    TagPresent(TagUid),
    TagAbsent,
    /// Pack voltage in millivolts, and whether the box was drawing playback
    /// current when it was sampled.
    Battery {
        pack_mv: u16,
        under_load: bool,
    },
    Charger(bool),
    /// The current track reached its end.
    TrackFinished,
    /// The story played to its end. Sent by the playback task.
    PlaybackEnded,
    /// Content for this tag is now available locally.
    ContentReady(TagUid),
    /// What the server said about a cached story, in answer to
    /// [`Action::Revalidate`].
    Revalidated(TagUid, Freshness),
    /// Content for this tag could not be obtained, and why.
    ContentMissing(TagUid, Unavailable),
    /// The box was slapped on one side. The accelerometer detects and latches
    /// it, so this arrives once per slap, however late it is read.
    Slap(Side),
    /// A plug went into the headphone socket, or came out of it.
    ///
    /// Sent by the task that polls the codec's headset-detect register. The
    /// socket has no switch the ESP32 can read, and no codec interrupt pin for
    /// it is known to work on this board.
    Headphones(bool),
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
    /// Where the sound should go. The firmware only mutes or unmutes the
    /// speaker. It does not change its own record of what is plugged in; only
    /// the headphone-detect code writes that.
    SetOutput(Output),
    /// The level to play at, as both the volume step and the codec's dB.
    ///
    /// The dB value is included so the firmware does not need to know which
    /// output is active to convert the step; that would be a second copy of
    /// state that could disagree with the reducer. The step is included for
    /// the console output and for tests.
    SetVolume {
        step: Volume,
        db: i8,
    },
    SetLed(LedState),
    SavePosition {
        tag: TagUid,
        pos: Position,
    },
    RequestContent(TagUid),
    /// Ask the server whether the story cached for this figure is still
    /// current.
    ///
    /// Answered by [`Event::Revalidated`]. Nothing plays until then: replacing
    /// a file while it plays would need two open handles on one file and a
    /// rename, and the SD card library supports neither.
    Revalidate(TagUid),
    /// Stop a download that is no longer needed.
    AbortFetch,
    PlayPrompt(Prompt),
    PowerOff(PowerOffReason),
}

#[derive(Debug, Clone, Copy)]
pub struct CoreConfig {
    pub volume_limit: u8,
    pub battery: BatteryConfig,
    /// Milliseconds of inactivity after which the box powers itself off.
    pub idle_timeout_ms: Millis,
}

impl Default for CoreConfig {
    fn default() -> Self {
        Self {
            volume_limit: MAX_VOLUME,
            battery: BatteryConfig::default(),
            idle_timeout_ms: 5 * 60 * 1_000,
        }
    }
}

#[derive(Debug)]
pub struct Core {
    config: CoreConfig,
    /// One per [`Output`], indexed by it. Each output keeps its own volume, so
    /// turning the headphones down does not change the speaker's level.
    volumes: [VolumeModel; 2],
    output: Output,
    battery: BatteryModel,
    playback: Playback,
    charging: bool,
    ear_down: [Option<Millis>; 2],
    /// Set when a press has already been used (for a chapter skip or a
    /// volume step), so the release that follows does nothing.
    ear_spent: [bool; 2],
    led: LedState,
    last_activity: Millis,
    last_tick: Millis,
    /// Set once the empty-battery shutdown has been requested, so it is
    /// requested once and not on every later battery reading.
    announced_shutdown: bool,
    /// Set once the idle timeout has asked the box to park, so it asks once
    /// and not on every tick.
    ///
    /// Never cleared: parking cuts the power rails, and only a reset (which
    /// creates a new `Core`) brings the box back.
    asked_to_park: bool,
    /// Set by the firmware when something the reducer cannot see is keeping
    /// the box in use. See [`Core::note_in_use`].
    externally_in_use: bool,
    /// Whether a held ear skips a chapter. See [`Core::note_ears_skip`].
    ears_skip: bool,
}

impl Core {
    pub fn new(config: CoreConfig) -> Self {
        Self {
            config,
            volumes: [VolumeModel::new(config.volume_limit); 2],
            output: Output::Speaker,
            battery: BatteryModel::new(config.battery),
            playback: Playback::new(),
            charging: false,
            ear_down: [None, None],
            ear_spent: [false, false],
            ears_skip: true,
            led: LedState::Booting,
            last_activity: 0,
            last_tick: 0,
            announced_shutdown: false,
            asked_to_park: false,
            externally_in_use: false,
        }
    }

    pub fn volume(&self) -> Volume {
        self.volumes[self.output as usize].current()
    }

    pub fn output(&self) -> Output {
        self.output
    }

    /// Records how far the story has got, so lifting the figure can save it.
    ///
    /// Not an `Event`: it makes no decision and is called once per decoded
    /// frame, far more often than anything else. It only updates RAM; the
    /// firmware writes the position to the card.
    pub fn note_position(&mut self, pos: Position) {
        self.playback.note_position(pos);
    }

    /// Records that something the reducer cannot see is keeping the box in
    /// use, so the idle timeout does not park it in the middle of a job.
    ///
    /// Not an `Event`, for the same reason as [`Core::note_position`]. The
    /// reducer only knows about figures on the plate. A console `play` or
    /// `taf` runs while the reducer is `Idle`, and a `batlog` run can last
    /// hours without making a sound.
    pub fn note_in_use(&mut self, in_use: bool) {
        self.externally_in_use = in_use;
    }

    /// Steps the volume for one ear, or plays the limit prompt if the volume
    /// is already at its end.
    ///
    /// Called from both the press and the release (depending on whether
    /// skipping is on), so both behave the same at the limit.
    fn step_volume(&mut self, ear: Ear, actions: &mut Actions) {
        let output = self.output;
        let model = &mut self.volumes[output as usize];
        let changed = match ear {
            Ear::Larger => model.up(),
            Ear::Smaller => model.down(),
        };
        match changed {
            Some(step) => {
                let _ = actions.push(Action::SetVolume {
                    step,
                    db: db_for(output, step),
                });
            }
            None => {
                let _ = actions.push(Action::PlayPrompt(Prompt::VolumeLimit));
            }
        }
    }

    /// Sets whether holding an ear skips a chapter (a setting on the card).
    ///
    /// When holding can skip, a press must wait until release to know it was
    /// a tap. When skipping is off, the volume changes on the press, like a
    /// stock box.
    pub fn note_ears_skip(&mut self, skips: bool) {
        self.ears_skip = skips;
    }

    /// Whether the box is busy with anything.
    ///
    /// A download counts: at about 42.5 KB/s, a 37 MB story takes about 15
    /// minutes, longer than the 5-minute idle timeout.
    ///
    /// Charging counts because the charger cannot wake the box, so a box
    /// parked while plugged in could not be woken by it.
    fn in_use(&self) -> bool {
        self.charging
            || self.externally_in_use
            || matches!(
                self.playback.kind(),
                PlaybackKind::Playing | PlaybackKind::Fetching
            )
    }

    pub fn handle<I: ContentIndex>(&mut self, event: Event, index: &I) -> Actions {
        let mut actions = Actions::new();

        // Events with a timestamp use it; the others use the last tick, the
        // only clock the core has. Battery and charger events are not
        // activity: the box samples its battery even when unused, and counting
        // that would keep it awake forever.
        match event {
            Event::Tick(now) => self.last_tick = now,
            Event::EarDown(_, at) | Event::EarHeld(_, at) | Event::EarUp(_, at) => {
                self.last_activity = at;
            }
            Event::TagPresent(_)
            | Event::TagAbsent
            | Event::ContentReady(_)
            | Event::Revalidated(..)
            | Event::ContentMissing(..)
            | Event::PlaybackEnded
            | Event::Slap(_)
            // Somebody plugged or unplugged something, so this is activity.
            | Event::Headphones(_) => {
                self.last_activity = self.last_tick;
            }
            Event::Battery { .. } | Event::Charger(_) | Event::TrackFinished => {}
        }

        match event {
            Event::Tick(now) => {
                // While the box is in use, keep moving the last-activity time
                // forward. Otherwise a job longer than the timeout would power
                // the box off the moment it ended.
                if self.in_use() {
                    self.last_activity = now;
                } else if !self.asked_to_park
                    && now.saturating_sub(self.last_activity) >= self.config.idle_timeout_ms
                {
                    self.asked_to_park = true;
                    let _ = actions.push(Action::PowerOff(PowerOffReason::Idle));
                }
            }

            Event::EarDown(ear, at) => {
                self.ear_down[ear as usize] = Some(at);
                // With skipping off, the press changes the volume right away
                // and the release does nothing, so one press is one step.
                self.ear_spent[ear as usize] = !self.ears_skip;
                if !self.ears_skip {
                    self.step_volume(ear, &mut actions);
                }
            }

            // A hold skips a chapter while the ear is still down. Only one
            // skip per press, however long the ear is held.
            Event::EarHeld(ear, _) => {
                if self.ear_down[ear as usize].is_none() || self.ear_spent[ear as usize] {
                    return actions;
                }
                self.ear_spent[ear as usize] = true;
                let _ = actions.push(match ear {
                    Ear::Larger => Action::NextTrack,
                    Ear::Smaller => Action::PrevTrack,
                });
            }

            Event::EarUp(ear, _) => {
                let was_down = self.ear_down[ear as usize].take().is_some();
                // Ignore a release without a press.
                if !was_down {
                    return actions;
                }
                // The press was already used (for a skip or a volume step).
                if self.ear_spent[ear as usize] {
                    self.ear_spent[ear as usize] = false;
                    return actions;
                }
                self.step_volume(ear, &mut actions);
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

            Event::Revalidated(tag, freshness) => {
                let a = self.playback.on_revalidated(tag, freshness, index);
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

            Event::Battery {
                pack_mv,
                under_load,
            } => {
                // Warn when the level settles into Low or Critical. Warning on
                // both catches a fast drop that skips Low.
                if let Some(BatteryLevel::Low | BatteryLevel::Critical) =
                    self.battery.update(pack_mv, under_load)
                {
                    let _ = actions.push(Action::PlayPrompt(Prompt::BatteryLow));
                }

                // Shut down at the hard cutoff, not at Critical.
                // `must_shut_down` stays true once set (a pack whose voltage
                // recovers without load is still empty), so only act on the
                // first time it becomes true.
                if self.battery.must_shut_down() && !self.announced_shutdown {
                    self.announced_shutdown = true;
                    let _ = actions.push(Action::PlayPrompt(Prompt::BatteryCritical));
                    let _ = actions.push(Action::PowerOff(PowerOffReason::PackEmpty));
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

            Event::Slap(side) => {
                let _ = actions.push(match side {
                    // Left goes back, right goes forward, like media player
                    // controls. This matches the ears: the larger ear is on
                    // the right and also skips forward.
                    Side::Left => Action::PrevTrack,
                    Side::Right => Action::NextTrack,
                });
            }

            // Change the output and its volume, nothing else. The story keeps
            // playing, so a child who pulls the plug does not lose their
            // place.
            Event::Headphones(plugged) => {
                self.output = if plugged {
                    Output::Headphones
                } else {
                    Output::Speaker
                };
                let _ = actions.push(Action::SetOutput(self.output));
                let step = self.volumes[self.output as usize].current();
                let _ = actions.push(Action::SetVolume {
                    step,
                    db: db_for(self.output, step),
                });
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
        fn wants_revalidation(&self, _tag: TagUid) -> bool {
            false
        }
    }

    /// A card with nothing on it, so a test can reach `State::Fetching`.
    struct Unknown;
    impl ContentIndex for Unknown {
        fn is_available(&self, _tag: TagUid) -> bool {
            false
        }
        fn saved_position(&self, _tag: TagUid) -> Position {
            Position::Start
        }
        fn wants_revalidation(&self, _tag: TagUid) -> bool {
            false
        }
    }

    /// A core with fixed battery thresholds, not the shipped calibration.
    ///
    /// These tests are about what the core does with a battery level, not
    /// about the real pack's voltages, so recalibrating the pack must not
    /// change them.
    fn core() -> Core {
        Core::new(CoreConfig {
            battery: BatteryConfig {
                full_mv: 3_900,
                ok_mv: 3_500,
                low_mv: 3_200,
                cutoff_mv: 3_000,
                load_offset_mv: 150,
                hysteresis_mv: 60,
                readings_to_agree: 4,
            },
            ..CoreConfig::default()
        })
    }

    fn contains(actions: &Actions, wanted: Action) -> bool {
        actions.contains(&wanted)
    }

    /// Lifting a figure saves the position the story had reached, not the one
    /// it started at.
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

    /// The larger ear turns the volume up, as on a stock box.
    #[test]
    fn a_tap_on_the_larger_ear_raises_the_volume() {
        let mut c = core();
        let before = c.volume();
        c.handle(Event::EarDown(Ear::Larger, 0), &Index);
        let actions = c.handle(Event::EarUp(Ear::Larger, 100), &Index);
        assert!(c.volume() > before);
        assert!(contains(
            &actions,
            Action::SetVolume {
                step: c.volume(),
                db: db_for(Output::Speaker, c.volume())
            }
        ));
    }

    #[test]
    fn a_tap_on_the_smaller_ear_lowers_the_volume() {
        let mut c = core();
        let before = c.volume();
        c.handle(Event::EarDown(Ear::Smaller, 0), &Index);
        c.handle(Event::EarUp(Ear::Smaller, 100), &Index);
        assert!(c.volume() < before);
    }

    /// With skipping off, a press cannot be a skip, so there is no reason to
    /// wait for the release.
    #[test]
    fn with_skipping_off_a_press_changes_the_volume_at_once() {
        let mut c = core();
        c.note_ears_skip(false);
        let before = c.volume();
        let actions = c.handle(Event::EarDown(Ear::Larger, 0), &Index);
        assert!(c.volume() > before);
        assert!(contains(
            &actions,
            Action::SetVolume {
                step: c.volume(),
                db: db_for(Output::Speaker, c.volume())
            }
        ));
    }

    #[test]
    fn with_skipping_off_the_release_does_nothing_more() {
        let mut c = core();
        c.note_ears_skip(false);
        c.handle(Event::EarDown(Ear::Larger, 0), &Index);
        let stepped = c.volume();
        let actions = c.handle(Event::EarUp(Ear::Larger, 100), &Index);
        assert!(actions.is_empty(), "{actions:?}");
        assert_eq!(c.volume(), stepped, "one press, one step");
    }

    /// With skipping on, a press might become a hold, so the volume waits for
    /// the release. Otherwise every skip would also change the volume.
    #[test]
    fn with_skipping_on_a_press_still_waits_for_the_release() {
        let mut c = core();
        let before = c.volume();
        let actions = c.handle(Event::EarDown(Ear::Larger, 0), &Index);
        assert_eq!(c.volume(), before);
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, Action::SetVolume { .. })),
            "{actions:?}"
        );
    }

    #[test]
    fn with_skipping_off_a_press_at_the_ceiling_prompts() {
        let mut c = core();
        c.note_ears_skip(false);
        for i in 0..10 {
            c.handle(Event::EarDown(Ear::Larger, i * 200), &Index);
            c.handle(Event::EarUp(Ear::Larger, i * 200 + 100), &Index);
        }
        let actions = c.handle(Event::EarDown(Ear::Larger, 5_000), &Index);
        assert!(contains(&actions, Action::PlayPrompt(Prompt::VolumeLimit)));
    }

    /// The skip happens while the ear is still held. Waiting for the release
    /// makes the box feel unresponsive.
    #[test]
    fn a_hold_skips_while_the_ear_is_still_down() {
        let mut c = core();
        c.handle(Event::EarDown(Ear::Larger, 0), &Index);
        let forward = c.handle(Event::EarHeld(Ear::Larger, 600), &Index);
        assert!(contains(&forward, Action::NextTrack));

        c.handle(Event::EarDown(Ear::Smaller, 2_000), &Index);
        let back = c.handle(Event::EarHeld(Ear::Smaller, 2_600), &Index);
        assert!(contains(&back, Action::PrevTrack));
    }

    /// The press was used for the skip, so the release must not also change
    /// the volume.
    #[test]
    fn the_release_after_a_hold_does_nothing() {
        let mut c = core();
        let before = c.volume();
        c.handle(Event::EarDown(Ear::Larger, 0), &Index);
        c.handle(Event::EarHeld(Ear::Larger, 600), &Index);
        let actions = c.handle(Event::EarUp(Ear::Larger, 2_000), &Index);
        assert!(actions.is_empty(), "{actions:?}");
        assert_eq!(c.volume(), before);
    }

    /// One hold skips one chapter, however long the ear is held.
    #[test]
    fn holding_on_after_the_skip_does_not_skip_again() {
        let mut c = core();
        c.handle(Event::EarDown(Ear::Larger, 0), &Index);
        c.handle(Event::EarHeld(Ear::Larger, 600), &Index);
        let again = c.handle(Event::EarHeld(Ear::Larger, 1_200), &Index);
        assert!(!contains(&again, Action::NextTrack), "{again:?}");
    }

    /// The firmware decides when a press becomes a hold. A release with no
    /// `EarHeld` before it is a tap, however long it took.
    #[test]
    fn a_release_with_no_hold_before_it_is_a_tap_however_long_it_took() {
        let mut c = core();
        let before = c.volume();
        c.handle(Event::EarDown(Ear::Larger, 0), &Index);
        let actions = c.handle(Event::EarUp(Ear::Larger, 10_000), &Index);
        assert!(c.volume() > before);
        assert!(contains(
            &actions,
            Action::SetVolume {
                step: c.volume(),
                db: db_for(Output::Speaker, c.volume())
            }
        ));
        assert!(!contains(&actions, Action::NextTrack));
    }

    #[test]
    fn a_hold_without_a_press_is_ignored() {
        let mut c = core();
        assert!(c
            .handle(Event::EarHeld(Ear::Larger, 600), &Index)
            .is_empty());
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

    /// The larger ear is on the box's right, and right goes forward, like a
    /// slap on that side. See `teddiebox_board::side_for_click`.
    #[test]
    fn a_long_press_on_the_right_ear_skips_forward() {
        let mut c = core();
        c.handle(Event::EarDown(Ear::Larger, 0), &Index);
        let actions = c.handle(Event::EarHeld(Ear::Larger, 600), &Index);
        assert!(contains(&actions, Action::NextTrack));
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, Action::SetVolume { .. })),
            "{actions:?}"
        );
    }

    #[test]
    fn a_long_press_on_the_left_ear_skips_backward() {
        let mut c = core();
        c.handle(Event::EarDown(Ear::Smaller, 0), &Index);
        let actions = c.handle(Event::EarHeld(Ear::Smaller, 600), &Index);
        assert!(contains(&actions, Action::PrevTrack));
    }

    #[test]
    fn a_release_without_a_press_is_ignored() {
        let mut c = core();
        assert!(c.handle(Event::EarUp(Ear::Larger, 100), &Index).is_empty());
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
        assert!(contains(&actions, Action::PowerOff(PowerOffReason::Idle)));
    }

    /// The idle timeout asks to park once, not on every tick after it expires.
    #[test]
    fn the_idle_park_is_asked_for_once_not_on_every_tick() {
        let mut c = core();
        let first = c.handle(Event::Tick(5 * 60 * 1_000 + 1), &Index);
        assert!(contains(&first, Action::PowerOff(PowerOffReason::Idle)));

        let again = c.handle(Event::Tick(5 * 60 * 1_000 + 2_000), &Index);
        assert!(
            !contains(&again, Action::PowerOff(PowerOffReason::Idle)),
            "a box already told to park must not be told again on the next tick"
        );
    }

    #[test]
    fn the_box_does_not_power_off_while_playing() {
        let mut c = core();
        c.handle(Event::TagPresent(TAG), &Index);
        let actions = c.handle(Event::Tick(5 * 60 * 1_000 + 1), &Index);
        assert!(!contains(&actions, Action::PowerOff(PowerOffReason::Idle)));
    }

    /// A child walks away and leaves the figure on the plate. Once the story
    /// ends, the idle timeout must still fire.
    #[test]
    fn a_finished_story_lets_the_idle_timeout_fire_with_the_figure_still_on() {
        let mut c = Core::new(CoreConfig::default());
        c.handle(Event::Tick(0), &Index);
        c.handle(Event::TagPresent(TagUid([1, 2, 3, 4, 5, 6, 7, 8])), &Index);
        c.handle(Event::PlaybackEnded, &Index);

        let actions = c.handle(Event::Tick(5 * 60 * 1_000 + 1), &Index);
        assert!(contains(&actions, Action::PowerOff(PowerOffReason::Idle)));
    }

    /// A story longer than the idle timeout must not switch the box off the
    /// moment it ends: the countdown starts when the box becomes idle, not when
    /// the figure was placed.
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
            !contains(&actions, Action::PowerOff(PowerOffReason::Idle)),
            "one second after the end is not idle"
        );

        let actions = c.handle(Event::Tick(30 * 60 * 1_000 + 5 * 60 * 1_000 + 1), &Index);
        assert!(
            contains(&actions, Action::PowerOff(PowerOffReason::Idle)),
            "five minutes after the end is"
        );
    }

    /// A child puts an unknown figure on the plate. At about 42.5 KB/s, a 37 MB
    /// story takes about 15 minutes to download, longer than the idle timeout,
    /// and `ContentReady` only arrives at the end. Parking would cut power
    /// while the card is being written.
    #[test]
    fn a_download_in_progress_holds_the_box_awake() {
        let mut c = core();
        c.handle(Event::Tick(0), &Unknown);
        let actions = c.handle(Event::TagPresent(TAG), &Unknown);
        assert!(contains(&actions, Action::RequestContent(TAG)));

        let actions = c.handle(Event::Tick(15 * 60 * 1_000), &Unknown);
        assert!(!contains(&actions, Action::PowerOff(PowerOffReason::Idle)));
    }

    /// A console `play` or `taf` runs while the reducer is `Idle`, and a
    /// `batlog` run can last hours without a sound. The firmware reports these
    /// so the box stays awake.
    #[test]
    fn something_only_the_firmware_can_see_holds_the_box_awake() {
        let mut c = core();
        c.handle(Event::Tick(0), &Index);
        c.note_in_use(true);

        let actions = c.handle(Event::Tick(60 * 60 * 1_000), &Index);
        assert!(!contains(&actions, Action::PowerOff(PowerOffReason::Idle)));

        c.note_in_use(false);
        let actions = c.handle(Event::Tick(60 * 60 * 1_000 + 1_000), &Index);
        assert!(
            !contains(&actions, Action::PowerOff(PowerOffReason::Idle)),
            "the countdown starts when it ended"
        );

        let actions = c.handle(Event::Tick(65 * 60 * 1_000 + 1), &Index);
        assert!(contains(&actions, Action::PowerOff(PowerOffReason::Idle)));
    }

    /// The charger cannot wake the box, so a box parked while charging
    /// overnight could not be woken by it.
    #[test]
    fn a_charging_box_does_not_park_itself() {
        let mut c = core();
        c.handle(Event::Tick(0), &Index);
        c.handle(Event::Charger(true), &Index);

        let actions = c.handle(Event::Tick(5 * 60 * 1_000 + 1), &Index);
        assert!(!contains(&actions, Action::PowerOff(PowerOffReason::Idle)));
    }

    /// Unplugging the charger is not activity, but it starts the idle
    /// countdown, because charging kept the box in use until then.
    #[test]
    fn unplugging_the_charger_starts_the_countdown() {
        let mut c = core();
        c.handle(Event::Tick(0), &Index);
        c.handle(Event::Charger(true), &Index);
        c.handle(Event::Tick(60 * 60 * 1_000), &Index);
        c.handle(Event::Charger(false), &Index);

        let actions = c.handle(Event::Tick(60 * 60 * 1_000 + 1_000), &Index);
        assert!(!contains(&actions, Action::PowerOff(PowerOffReason::Idle)));

        let actions = c.handle(Event::Tick(65 * 60 * 1_000 + 1), &Index);
        assert!(contains(&actions, Action::PowerOff(PowerOffReason::Idle)));
    }

    #[test]
    fn activity_defers_the_power_off() {
        let mut c = core();
        c.handle(Event::EarDown(Ear::Larger, 4 * 60 * 1_000), &Index);
        c.handle(Event::EarUp(Ear::Larger, 4 * 60 * 1_000 + 100), &Index);
        let actions = c.handle(Event::Tick(5 * 60 * 1_000 + 1), &Index);
        assert!(!contains(&actions, Action::PowerOff(PowerOffReason::Idle)));
    }

    /// The box announces the shutdown, then shuts down, each exactly once and
    /// not again on later readings.
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
            if contains(&actions, Action::PowerOff(PowerOffReason::PackEmpty)) {
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
        assert!(
            !contains(&actions, Action::PowerOff(PowerOffReason::PackEmpty)),
            "and not again after"
        );
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
    /// 3000. Between them the box warns but does not shut down.
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
            !contains(&actions_seen, Action::PowerOff(PowerOffReason::PackEmpty)),
            "but 3100 mV is above the 3000 mV cutoff, so nothing stops"
        );
        assert!(
            !contains(&actions_seen, Action::PlayPrompt(Prompt::BatteryCritical)),
            "and announcing a shutdown that is not happening would be a lie"
        );
    }

    /// Settling into Low warns once; falling further into Critical warns
    /// again.
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

    /// A recovering pack makes no sound, but a later drop warns again.
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

        // 3_700 is above the Ok threshold plus hysteresis, so the level rises
        // again, silently.
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
    fn the_indicator_is_not_reset_when_nothing_changed() {
        let mut c = core();
        c.handle(Event::TagPresent(TAG), &Index);
        let actions = c.handle(Event::Tick(1_000), &Index);
        assert!(!actions.iter().any(|a| matches!(a, Action::SetLed(_))));
    }

    #[test]
    fn a_slap_on_the_right_goes_to_the_next_chapter() {
        let mut c = core();
        let actions = c.handle(Event::Slap(Side::Right), &Index);
        assert!(contains(&actions, Action::NextTrack));
    }

    #[test]
    fn a_slap_on_the_left_goes_back_a_chapter() {
        let mut c = core();
        let actions = c.handle(Event::Slap(Side::Left), &Index);
        assert!(contains(&actions, Action::PrevTrack));
    }

    /// The headphone socket does not switch the speaker off by itself, so the
    /// box has to.
    #[test]
    fn a_jack_going_in_moves_the_sound_to_the_headphones() {
        let mut c = core();
        let actions = c.handle(Event::Headphones(true), &Index);
        assert!(contains(&actions, Action::SetOutput(Output::Headphones)));
        assert_eq!(c.output(), Output::Headphones);
    }

    /// Like a stock box: pulling the plug does not pause, stop or seek.
    #[test]
    fn a_jack_coming_out_brings_the_speaker_back_without_touching_playback() {
        let mut c = core();
        c.handle(Event::Headphones(true), &Index);
        let actions = c.handle(Event::Headphones(false), &Index);
        assert!(contains(&actions, Action::SetOutput(Output::Speaker)));
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, Action::Pause | Action::Stop | Action::SeekTo(_))),
            "{actions:?}"
        );
    }

    /// The volume changes with the output, or the headphones would start at
    /// the speaker's level.
    #[test]
    fn plugging_in_asks_for_the_headphone_ladders_level() {
        let mut c = core();
        let step = c.volume();
        let actions = c.handle(Event::Headphones(true), &Index);
        assert!(contains(
            &actions,
            Action::SetVolume {
                step,
                db: db_for(Output::Headphones, step)
            }
        ));
    }

    /// Each output keeps its own volume. Turning the headphones down must not
    /// make the speaker quiet, and a loud speaker must not carry over to the
    /// headphones.
    #[test]
    fn each_output_remembers_its_own_step_across_a_plug_and_an_unplug() {
        let mut c = core();
        let speaker_step = c.volume();

        c.handle(Event::Headphones(true), &Index);
        c.handle(Event::EarDown(Ear::Smaller, 0), &Index);
        c.handle(Event::EarUp(Ear::Smaller, 100), &Index);
        let headphone_step = c.volume();
        assert!(headphone_step < speaker_step);

        let back = c.handle(Event::Headphones(false), &Index);
        assert_eq!(c.volume(), speaker_step, "the speaker kept its own step");
        assert!(contains(
            &back,
            Action::SetVolume {
                step: speaker_step,
                db: db_for(Output::Speaker, speaker_step)
            }
        ));

        let again = c.handle(Event::Headphones(true), &Index);
        assert_eq!(c.volume(), headphone_step, "and so did the headphones");
        assert!(contains(
            &again,
            Action::SetVolume {
                step: headphone_step,
                db: db_for(Output::Headphones, headphone_step)
            }
        ));
    }

    /// With headphones in, the ears change the headphone volume.
    #[test]
    fn an_ear_steps_the_ladder_of_whatever_is_plugged_in() {
        let mut c = core();
        c.handle(Event::Headphones(true), &Index);
        c.handle(Event::EarDown(Ear::Larger, 0), &Index);
        let actions = c.handle(Event::EarUp(Ear::Larger, 100), &Index);
        assert!(contains(
            &actions,
            Action::SetVolume {
                step: c.volume(),
                db: db_for(Output::Headphones, c.volume())
            }
        ));
    }

    /// Plugging in headphones counts as activity.
    #[test]
    fn plugging_headphones_in_restarts_the_idle_countdown() {
        let mut c = core();
        c.handle(Event::Tick(4 * 60 * 1_000), &Index);
        c.handle(Event::Headphones(true), &Index);
        let actions = c.handle(Event::Tick(5 * 60 * 1_000 + 1), &Index);
        assert!(!contains(&actions, Action::PowerOff(PowerOffReason::Idle)));
    }
}
