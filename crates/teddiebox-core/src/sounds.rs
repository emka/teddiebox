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
    /// A short alert. **This is what a low battery plays on this box**, going
    /// by ear, though the wiki lists the spoken warning at `00000009`; the two
    /// are a chime and a sentence, and both are reasonably called the
    /// battery-low sound.
    Alert,
    /// The spoken low-battery message asking to be charged.
    LowBattery,
    /// "Now I'm ready for the Tonies."
    Ready,
    /// A download was interrupted.
    NetworkError,
}

impl Sound {
    pub const fn file(self) -> u32 {
        match self {
            Self::Startup => 0x0000_0000,
            Self::Confirmation => 0x0000_0001,
            Self::Alert => 0x0000_0003,
            Self::LowBattery => 0x0000_0009,
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
}
