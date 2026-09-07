//! The box's own voice: the audio it plays about itself.
//!
//! A Toniebox ships four copies of its system sounds under `CONTENT/`, one per
//! language, numbered `00000000` to `00000003`. Everything else under
//! `CONTENT/` is a figure, one file per directory. Which language a box speaks
//! is a property of the box rather than of the card, so it is chosen at build
//! time from `TEDDIEBOX_LANGUAGE` in `.envrc`.
//!
//! Mapping from the Toniebox wiki's list of internal audio files, confirmed on
//! this card by the file counts: 22, 25, 22 and 22 files against exactly one
//! for every figure.

use crate::power::PackState;

/// Which set of system sounds the box speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    EnglishGb,
    German,
    EnglishUs,
    French,
}

impl Language {
    /// Reads the name used in `.envrc`.
    ///
    /// `const` so a wrong value fails the build rather than the box: a
    /// mistyped language would otherwise be a silent fall back to whichever
    /// default seemed reasonable, and the first sign of it would be the box
    /// speaking the wrong language to a child.
    pub const fn from_name(name: &str) -> Option<Self> {
        // `match` on strings is not const, so compare bytes.
        const fn eq(a: &[u8], b: &[u8]) -> bool {
            if a.len() != b.len() {
                return false;
            }
            let mut i = 0;
            while i < a.len() {
                if a[i] != b[i] {
                    return false;
                }
                i += 1;
            }
            true
        }
        let name = name.as_bytes();
        if eq(name, b"de") {
            Some(Self::German)
        } else if eq(name, b"en-gb") {
            Some(Self::EnglishGb)
        } else if eq(name, b"en-us") {
            Some(Self::EnglishUs)
        } else if eq(name, b"fr") {
            Some(Self::French)
        } else {
            None
        }
    }

    /// The `CONTENT/` directory holding this language's sounds.
    pub const fn content_directory(self) -> u32 {
        match self {
            Self::EnglishGb => 0x0000_0000,
            Self::German => 0x0000_0001,
            Self::EnglishUs => 0x0000_0002,
            Self::French => 0x0000_0003,
        }
    }
}

/// A sound the box plays about itself, as the file ID within a language.
///
/// Only the ones this firmware has a use for are named. The rest of each
/// directory is a series of error messages distinguished by a codeword
/// animal, which are worth naming when something needs to play one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sound {
    /// The jingle at power-on. Three seconds, and the reason a start-up can
    /// afford to power the speaker: it covers the transient with the sound
    /// stock covers it with.
    Startup,
    /// A short rising chime — "tada".
    Confirmation,
    /// "Caution, battery is low." A warning: the box keeps playing.
    ///
    /// Identified by ear on this box. The wiki lists this one only as a
    /// generic alert, so the pairing with `BatteryCritical` below comes from
    /// the bench rather than from the table.
    BatteryLow,
    /// "Battery is critical, turning off now."
    ///
    /// Not a warning but an announcement, which makes it the one sound with an
    /// ordering requirement: it has to finish before the box powers down, or
    /// it says the box is turning off and then does not.
    BatteryCritical,
    /// "Now I'm ready for the Tonies."
    Ready,
    /// A download was interrupted.
    NetworkError,
    /// "Help me with the config."
    ///
    /// For a `/CONFIG.TXT` that is missing, truncated or malformed. The box
    /// keeps playing everything already on the card — a typo in a config file
    /// must never cost a child their story — but a parent with no serial cable
    /// needs some way to know, and this is the box's own word for it.
    ConfigError,
    /// "No Internet."
    ///
    /// The network could not be reached: association or DHCP failed, or the
    /// server did not answer.
    NoInternet,
    /// "Wrong password."
    ///
    /// Specifically the Wi-Fi passphrase. The bench proved a wrong one reports
    /// `FourWayHandshakeTimeout`, which is distinguishable from a network that
    /// is simply out of range — so the box can say which of the two it is
    /// rather than blaming the network for a typo.
    WrongPassword,
    /// The box reached the server and there is no story for this figure.
    ///
    /// Identified by ear on the bench, like the two battery sounds and for the
    /// same reason: the published mapping has been wrong before. It sits
    /// between `NoInternet` and `WrongPassword` on the card, which is the sort
    /// of coincidence that makes a wrong guess plausible — so this one is
    /// pinned by its own test.
    NoStory,
}

impl Sound {
    /// What the box should say about a pack in this state, if anything.
    ///
    /// The three pack states and the two battery sounds are the same three
    /// cases, so they are paired here once rather than at each place that
    /// notices a flat battery. Getting it wrong is not a silent bug: it tells
    /// a child the box is turning off when it is not, or fails to tell them
    /// when it is.
    pub const fn for_pack_state(state: crate::power::PackState) -> Option<Self> {
        match state {
            crate::power::PackState::Healthy => None,
            crate::power::PackState::Low => Some(Self::BatteryLow),
            crate::power::PackState::Critical => Some(Self::BatteryCritical),
        }
    }

    /// What the box should say for a prompt, if it has words for it.
    ///
    /// `None` is a real answer and not an oversight. Every sound here was
    /// identified by ear on this card, because the published mapping has been
    /// wrong before, and the two prompts that still return `None` are the two
    /// nobody has listened for. Pointing one of them at a sound that says
    /// something else would have the box say something true about the wrong
    /// thing, which is the failure this module guards against elsewhere.
    pub const fn for_prompt(prompt: crate::Prompt) -> Option<Self> {
        match prompt {
            crate::Prompt::Startup => Some(Self::Startup),
            crate::Prompt::NoNetwork => Some(Self::NoInternet),
            crate::Prompt::BatteryLow => Some(Self::BatteryLow),
            crate::Prompt::NoContent => Some(Self::NoStory),
            crate::Prompt::Shutdown | crate::Prompt::VolumeLimit => None,
        }
    }

    pub const fn file(self) -> u32 {
        match self {
            Self::Startup => 0x0000_0000,
            Self::Confirmation => 0x0000_0001,
            Self::BatteryLow => 0x0000_0003,
            Self::BatteryCritical => 0x0000_0009,
            Self::Ready => 0x0000_0010,
            Self::NetworkError => 0x0000_000F,
            Self::ConfigError => 0x0000_000B,
            Self::NoInternet => 0x0000_0011,
            Self::WrongPassword => 0x0000_0013,
            Self::NoStory => 0x0000_0012,
        }
    }
}

/// How many readings must agree before the box says anything about the pack.
///
/// The pack reading has been implausible before — the very first sample this
/// project took was 9453 mV from three NiMH cells — and a single bad one must
/// not make the box announce that it is turning off. Four readings at the
/// battery task's interval is a few seconds, which is nothing against a
/// discharge curve.
pub const READINGS_TO_AGREE: u8 = 4;

/// Turns a stream of pack readings into the few moments worth speaking about.
///
/// The box should say something when the pack *becomes* low or critical, once,
/// not on every reading — and never on the strength of one sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Announcer {
    settled: PackState,
    candidate: PackState,
    agreed: u8,
}

impl Default for Announcer {
    fn default() -> Self {
        Self::new()
    }
}

impl Announcer {
    /// Starts healthy, so a box that boots with a healthy pack says nothing.
    pub const fn new() -> Self {
        Self {
            settled: PackState::Healthy,
            candidate: PackState::Healthy,
            agreed: 0,
        }
    }

    /// The state the box currently believes the pack is in.
    pub const fn settled(&self) -> PackState {
        self.settled
    }

    /// Feeds one reading, and says what to play if anything.
    pub fn observe(&mut self, state: PackState) -> Option<Sound> {
        if state == self.candidate {
            self.agreed = self.agreed.saturating_add(1);
        } else {
            self.candidate = state;
            self.agreed = 1;
        }

        if self.agreed < READINGS_TO_AGREE || state == self.settled {
            return None;
        }
        self.settled = state;
        // Recovery is not worth interrupting anyone for; only the way down.
        Sound::for_pack_state(state)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::vec::Vec;

    use super::*;
    use crate::Prompt;

    /// Spelled out, like the language directories and for the same reason: a
    /// table that agreed with itself could be wrong about every entry. These
    /// three were identified on the bench.
    #[test]
    fn the_failure_sounds_name_the_files_the_bench_identified() {
        assert_eq!(Sound::ConfigError.file(), 0x0000_000B);
        assert_eq!(Sound::NoInternet.file(), 0x0000_0011);
        assert_eq!(Sound::WrongPassword.file(), 0x0000_0013);
    }

    /// Two sounds sharing a file is the bug this catches: the box would say
    /// something true about the wrong thing, which is worse than silence and
    /// far harder to notice than a crash.
    #[test]
    fn no_two_sounds_share_a_file() {
        let all = [
            Sound::Startup,
            Sound::Confirmation,
            Sound::BatteryLow,
            Sound::BatteryCritical,
            Sound::Ready,
            Sound::NetworkError,
            Sound::ConfigError,
            Sound::NoInternet,
            Sound::WrongPassword,
        ];
        let mut seen: Vec<u32> = Vec::new();
        for sound in all {
            let file = sound.file();
            assert!(!seen.contains(&file), "{sound:?} reuses file {file:#010X}");
            seen.push(file);
        }
    }

    /// The four directories, spelled out rather than derived: these are the
    /// numbers on the card, and a table that agreed with itself could be
    /// wrong about every one of them.
    #[test]
    fn each_language_names_its_own_content_directory() {
        assert_eq!(Language::German.content_directory(), 0x0000_0001);
        assert_eq!(Language::EnglishGb.content_directory(), 0x0000_0000);
        assert_eq!(Language::EnglishUs.content_directory(), 0x0000_0002);
        assert_eq!(Language::French.content_directory(), 0x0000_0003);
    }

    #[test]
    fn the_envrc_names_map_to_languages() {
        assert_eq!(Language::from_name("de"), Some(Language::German));
        assert_eq!(Language::from_name("en-gb"), Some(Language::EnglishGb));
        assert_eq!(Language::from_name("en-us"), Some(Language::EnglishUs));
        assert_eq!(Language::from_name("fr"), Some(Language::French));
    }

    /// A mistyped language must not quietly become a working one. The box
    /// would then speak the wrong language to a child, and nothing about that
    /// looks like a configuration error.
    #[test]
    fn an_unknown_language_is_refused_rather_than_defaulted() {
        assert_eq!(Language::from_name("german"), None);
        assert_eq!(Language::from_name("DE"), None);
        assert_eq!(Language::from_name(""), None);
    }

    #[test]
    fn the_startup_sound_is_the_first_file_of_a_language() {
        assert_eq!(Sound::Startup.file(), 0x0000_0000);
    }

    /// The pack states and the battery sounds are the same three cases, so
    /// pairing them anywhere else is a chance to pair them wrongly.
    #[test]
    fn each_pack_state_names_the_sound_that_announces_it() {
        use crate::power::PackState;
        assert_eq!(
            Sound::for_pack_state(PackState::Low),
            Some(Sound::BatteryLow)
        );
        assert_eq!(
            Sound::for_pack_state(PackState::Critical),
            Some(Sound::BatteryCritical)
        );
        assert_eq!(
            Sound::for_pack_state(PackState::Healthy),
            None,
            "a healthy pack has nothing to announce"
        );
    }

    /// The two battery sounds say different things — "caution, battery is
    /// low" against "battery is critical, turning off now" — and playing the
    /// second when the first was meant tells a child the box is about to stop
    /// when it is not.
    #[test]
    fn the_two_battery_sounds_are_not_the_same_file() {
        assert_eq!(Sound::BatteryLow.file(), 0x0000_0003);
        assert_eq!(Sound::BatteryCritical.file(), 0x0000_0009);
    }

    /// A pack reading has been wildly wrong before, so one low sample must not
    /// announce anything — least of all that the box is turning off.
    #[test]
    fn a_single_low_reading_says_nothing() {
        let mut announcer = Announcer::new();
        assert_eq!(announcer.observe(PackState::Low), None);
        assert_eq!(announcer.settled(), PackState::Healthy);
    }

    #[test]
    fn a_pack_that_stays_low_is_announced_once() {
        let mut announcer = Announcer::new();
        let mut spoken = Vec::new();
        for _ in 0..10 {
            if let Some(sound) = announcer.observe(PackState::Low) {
                spoken.push(sound);
            }
        }
        assert_eq!(spoken, [Sound::BatteryLow], "once, not once a reading");
    }

    /// Readings that disagree restart the count: a flapping value is not four
    /// readings of anything.
    #[test]
    fn readings_must_agree_consecutively() {
        let mut announcer = Announcer::new();
        for _ in 0..3 {
            assert_eq!(announcer.observe(PackState::Low), None);
            assert_eq!(announcer.observe(PackState::Healthy), None);
        }
        assert_eq!(announcer.settled(), PackState::Healthy);
    }

    #[test]
    fn a_pack_that_falls_further_is_announced_again() {
        let mut announcer = Announcer::new();
        let mut spoken = Vec::new();
        for _ in 0..READINGS_TO_AGREE {
            spoken.extend(announcer.observe(PackState::Low));
        }
        for _ in 0..READINGS_TO_AGREE {
            spoken.extend(announcer.observe(PackState::Critical));
        }
        assert_eq!(spoken, [Sound::BatteryLow, Sound::BatteryCritical]);
    }

    /// Being plugged in is good news, and good news does not interrupt a
    /// story. It does re-arm the warning for the next discharge.
    #[test]
    fn recovering_is_silent_but_arms_the_warning_again() {
        let mut announcer = Announcer::new();
        let mut spoken = Vec::new();
        for _ in 0..READINGS_TO_AGREE {
            spoken.extend(announcer.observe(PackState::Low));
        }
        for _ in 0..READINGS_TO_AGREE {
            spoken.extend(announcer.observe(PackState::Healthy));
        }
        for _ in 0..READINGS_TO_AGREE {
            spoken.extend(announcer.observe(PackState::Low));
        }
        assert_eq!(spoken, [Sound::BatteryLow, Sound::BatteryLow]);
    }

    #[test]
    fn the_prompts_with_a_known_file_map_to_it() {
        assert_eq!(Sound::for_prompt(Prompt::Startup), Some(Sound::Startup));
        assert_eq!(
            Sound::for_prompt(Prompt::NoNetwork),
            Some(Sound::NoInternet)
        );
        assert_eq!(
            Sound::for_prompt(Prompt::BatteryLow),
            Some(Sound::BatteryLow)
        );
        assert_eq!(Sound::for_prompt(Prompt::NoContent), Some(Sound::NoStory));
    }

    /// The file a figure with no story gets. Written out rather than compared
    /// against the mapping, because the whole value of this identification is
    /// that it came from someone listening to the card.
    #[test]
    fn the_figure_with_no_story_has_its_own_file() {
        assert_eq!(Sound::NoStory.file(), 0x0000_0012);
    }

    /// Two prompts still have no file. Shutdown and the volume ceiling were
    /// never identified by ear, and guessing would have the box say something
    /// true about the wrong thing.
    #[test]
    fn the_prompts_with_no_identified_file_map_to_nothing() {
        assert_eq!(Sound::for_prompt(Prompt::Shutdown), None);
        assert_eq!(Sound::for_prompt(Prompt::VolumeLimit), None);
    }
}
