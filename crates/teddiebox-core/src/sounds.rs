//! The box's system sounds.
//!
//! A Toniebox card holds four sets of system sounds under `CONTENT/`, one per
//! language, in directories `00000000` to `00000003`. Everything else under
//! `CONTENT/` is a figure, one file per directory. The language is chosen at
//! build time from `TEDDIEBOX_LANGUAGE` in `.envrc`.
//!
//! The file IDs were identified by listening to them; the Toniebox wiki's
//! list is wrong in places.

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
    /// `const` so a wrong value fails the build instead of silently falling
    /// back to a default language.
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
/// Only the sounds the firmware uses are named. The rest are error messages,
/// each identified by an animal codeword.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sound {
    /// The three-second jingle at power-on. It hides the click of the speaker
    /// powering up, as on a stock box.
    Startup,
    /// A short rising chime — "tada".
    Confirmation,
    /// "Caution, battery is low." A warning: the box keeps playing.
    ///
    /// Identified by listening; the wiki lists it only as a generic alert.
    BatteryLow,
    /// "Battery is critical, turning off now."
    ///
    /// An announcement, not a warning, so it must finish playing before the
    /// box powers down.
    BatteryCritical,
    /// "Now I'm ready for the Tonies."
    Ready,
    /// A download was interrupted.
    NetworkError,
    /// "Help me with the config."
    ///
    /// For a `/CONFIG.TXT` that is missing, truncated or malformed. The box
    /// still plays everything already on the card; this tells a parent
    /// without a serial cable that the config needs fixing.
    ConfigError,
    /// "No Internet."
    ///
    /// The network could not be reached: association or DHCP failed, or the
    /// server did not answer.
    NoInternet,
    /// "Wrong password."
    ///
    /// The Wi-Fi passphrase. A wrong one shows up as
    /// `FourWayHandshakeTimeout`, which is different from a network being out
    /// of range, so the box can tell the two apart.
    WrongPassword,
    /// The box reached the server and there is no story for this figure.
    ///
    /// Identified by listening. It sits between `NoInternet` and
    /// `WrongPassword`, so it has its own test.
    NoStory,
}

impl Sound {
    /// The sound for a prompt, if there is one.
    ///
    /// `None` is deliberate. The volume limit has no sound: every file on the
    /// card was checked and none fits (the closest, `0x02`, is a
    /// discouraging "no", and was rejected).
    pub const fn for_prompt(prompt: crate::Prompt) -> Option<Self> {
        match prompt {
            crate::Prompt::Startup => Some(Self::Startup),
            crate::Prompt::NoNetwork => Some(Self::NoInternet),
            crate::Prompt::BatteryLow => Some(Self::BatteryLow),
            crate::Prompt::BatteryCritical => Some(Self::BatteryCritical),
            crate::Prompt::NoContent => Some(Self::NoStory),
            crate::Prompt::WrongPassword => Some(Self::WrongPassword),
            crate::Prompt::VolumeLimit => None,
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

#[cfg(test)]
mod tests {
    extern crate std;
    use std::vec::Vec;

    use super::*;
    use crate::Prompt;

    /// Written as literals, so the test can disagree with the table. These
    /// were identified by listening.
    #[test]
    fn the_failure_sounds_name_the_files_the_bench_identified() {
        assert_eq!(Sound::ConfigError.file(), 0x0000_000B);
        assert_eq!(Sound::NoInternet.file(), 0x0000_0011);
        assert_eq!(Sound::WrongPassword.file(), 0x0000_0013);
    }

    /// Two sounds sharing a file would make the box say the wrong thing,
    /// which is worse than silence and hard to notice.
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

    /// The four directories, written as literals so the test can disagree
    /// with the code.
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

    /// A mistyped language must be refused, not replaced with a default.
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

    /// "Caution, battery is low" and "battery is critical, turning off now"
    /// are different files.
    #[test]
    fn the_two_battery_sounds_are_not_the_same_file() {
        assert_eq!(Sound::BatteryLow.file(), 0x0000_0003);
        assert_eq!(Sound::BatteryCritical.file(), 0x0000_0009);
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
        assert_eq!(
            Sound::for_prompt(Prompt::BatteryCritical),
            Some(Sound::BatteryCritical)
        );
        assert_eq!(Sound::for_prompt(Prompt::NoContent), Some(Sound::NoStory));
        assert_eq!(
            Sound::for_prompt(Prompt::WrongPassword),
            Some(Sound::WrongPassword)
        );
    }

    /// Three neighbouring files: `no internet` at 0x11, `no story` at 0x12,
    /// `wrong password` at 0x13, identified by listening. A mix-up here would
    /// be easy to miss.
    #[test]
    fn a_refused_passphrase_and_an_absent_network_are_different_files() {
        assert_eq!(Sound::WrongPassword.file(), 0x0000_0013);
        assert_eq!(Sound::NoInternet.file(), 0x0000_0011);
        assert_ne!(Sound::WrongPassword.file(), Sound::NoInternet.file());
    }

    /// The sound for a figure with no story, identified by listening.
    #[test]
    fn the_figure_with_no_story_has_its_own_file() {
        assert_eq!(Sound::NoStory.file(), 0x0000_0012);
    }

    /// The volume limit is silent on purpose: no file on the card fits it.
    #[test]
    fn the_volume_ceiling_is_deliberately_silent() {
        assert_eq!(Sound::for_prompt(Prompt::VolumeLimit), None);
    }

    /// Identified by listening; the wiki is wrong about this one. 0x09 is
    /// "battery is critical, turning off now", not 0x03.
    #[test]
    fn the_critical_announcement_has_its_own_file() {
        assert_eq!(Sound::BatteryCritical.file(), 0x0000_0009);
        assert_ne!(Sound::BatteryCritical.file(), Sound::BatteryLow.file());
    }
}
