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
    /// What the box should say for a prompt, if it has words for it.
    ///
    /// `None` is a real answer and not an oversight. Every sound here was
    /// identified by ear on this card, because the published mapping has been
    /// wrong before — and so was the silence: all twenty-five German files were
    /// played and listened to on 2026-09-14 before the volume ceiling was left
    /// without one. The only candidate was `0x02`, the box's discouraging
    /// "no", and it was turned down. Pointing a prompt at a sound that says
    /// something else would have the box say something true about the wrong
    /// thing, which is the failure this module guards against elsewhere.
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

    /// The two battery sounds say different things — "caution, battery is
    /// low" against "battery is critical, turning off now" — and playing the
    /// second when the first was meant tells a child the box is about to stop
    /// when it is not.
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

    /// Three neighbouring files, and the box has to pick the right one of the
    /// three: `no internet` at 0x11, `no story` at 0x12, `wrong password` at
    /// 0x13. All three were identified by ear because the published mapping has
    /// been wrong before, and a wrong guess here is plausible rather than
    /// obvious — so the two a network failure can reach are pinned apart, by
    /// number, in one place.
    #[test]
    fn a_refused_passphrase_and_an_absent_network_are_different_files() {
        assert_eq!(Sound::WrongPassword.file(), 0x0000_0013);
        assert_eq!(Sound::NoInternet.file(), 0x0000_0011);
        assert_ne!(Sound::WrongPassword.file(), Sound::NoInternet.file());
    }

    /// The file a figure with no story gets. Written out rather than compared
    /// against the mapping, because the whole value of this identification is
    /// that it came from someone listening to the card.
    #[test]
    fn the_figure_with_no_story_has_its_own_file() {
        assert_eq!(Sound::NoStory.file(), 0x0000_0012);
    }

    /// The volume ceiling is silent by decision, not for want of looking.
    /// Every one of the card's twenty-five German files was played and listened
    /// to on 2026-09-14; the only candidate was `0x02`, the discouraging
    /// "tadum mhh mhh", and silence was chosen over it. A ceiling that says
    /// nothing is what this box does.
    #[test]
    fn the_volume_ceiling_is_deliberately_silent() {
        assert_eq!(Sound::for_prompt(Prompt::VolumeLimit), None);
    }

    /// Identified by ear, and the published wiki mapping was wrong about this
    /// one: 0x09 is "battery is critical, turning off now", not 0x03.
    #[test]
    fn the_critical_announcement_has_its_own_file() {
        assert_eq!(Sound::BatteryCritical.file(), 0x0000_0009);
        assert_ne!(Sound::BatteryCritical.file(), Sound::BatteryLow.file());
    }
}
