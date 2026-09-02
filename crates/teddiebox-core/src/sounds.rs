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

    pub const fn file(self) -> u32 {
        match self {
            Self::Startup => 0x0000_0000,
            Self::Confirmation => 0x0000_0001,
            Self::BatteryLow => 0x0000_0003,
            Self::BatteryCritical => 0x0000_0009,
            Self::Ready => 0x0000_0010,
            Self::NetworkError => 0x0000_000F,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
