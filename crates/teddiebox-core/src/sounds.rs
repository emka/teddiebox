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
    /// `WrongPassword`, so the three are easy to mix up.
    NoStory,
}

impl Sound {
    /// The sound for a prompt.
    pub const fn for_prompt(prompt: crate::Prompt) -> Self {
        match prompt {
            crate::Prompt::Startup => Self::Startup,
            crate::Prompt::NoNetwork => Self::NoInternet,
            crate::Prompt::BatteryLow => Self::BatteryLow,
            crate::Prompt::BatteryCritical => Self::BatteryCritical,
            crate::Prompt::NoContent => Self::NoStory,
            crate::Prompt::WrongPassword => Self::WrongPassword,
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
    use super::*;
    use crate::Prompt;

    /// Written as literals, so the test can disagree with the table. Every
    /// file was identified by listening; the Toniebox wiki is wrong in places.
    #[test]
    fn each_sound_names_the_file_the_bench_identified() {
        // Given
        let sounds = [
            Sound::Startup,
            Sound::Confirmation,
            Sound::BatteryLow,
            Sound::BatteryCritical,
            Sound::ConfigError,
            Sound::NetworkError,
            Sound::Ready,
            Sound::NoInternet,
            Sound::NoStory,
            Sound::WrongPassword,
        ];

        // When
        let files = sounds.map(Sound::file);

        // Then
        assert_eq!(
            files,
            [
                0x0000_0000, // the first file of a language
                0x0000_0001,
                0x0000_0003, // "caution, battery is low"
                0x0000_0009, // "turning off now"; the wiki says 0x03
                0x0000_000B,
                0x0000_000F,
                0x0000_0010,
                0x0000_0011, // 0x11 to 0x13 are neighbours, easy to mix up
                0x0000_0012,
                0x0000_0013,
            ]
        );
    }

    /// Two sounds sharing a file would make the box say the wrong thing,
    /// which is worse than silence and hard to notice.
    #[test]
    fn no_two_sounds_share_a_file() {
        // Given
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
            Sound::NoStory,
        ];

        // When
        let files = all.map(Sound::file);

        // Then
        for (i, file) in files.iter().enumerate() {
            assert!(
                !files[..i].contains(file),
                "{:?} reuses file {file:#010X}",
                all[i]
            );
        }
    }

    /// The four directories, written as literals so the test can disagree
    /// with the code.
    #[test]
    fn each_language_names_its_own_content_directory() {
        // Given
        let languages = [
            Language::German,
            Language::EnglishGb,
            Language::EnglishUs,
            Language::French,
        ];

        // When
        let directories = languages.map(Language::content_directory);

        // Then
        assert_eq!(
            directories,
            [0x0000_0001, 0x0000_0000, 0x0000_0002, 0x0000_0003]
        );
    }

    #[test]
    fn the_envrc_names_map_to_languages() {
        // Given
        let names = ["de", "en-gb", "en-us", "fr"];

        // When
        let languages = names.map(Language::from_name);

        // Then
        assert_eq!(
            languages,
            [
                Some(Language::German),
                Some(Language::EnglishGb),
                Some(Language::EnglishUs),
                Some(Language::French),
            ]
        );
    }

    /// A mistyped language must be refused, not replaced with a default.
    #[test]
    fn an_unknown_language_is_refused_rather_than_defaulted() {
        // Given
        let names = ["german", "DE", ""];

        // When
        let languages = names.map(Language::from_name);

        // Then
        assert_eq!(languages, [None; 3]);
    }

    #[test]
    fn each_prompt_maps_to_its_file() {
        // Given
        let prompts = [
            Prompt::Startup,
            Prompt::NoNetwork,
            Prompt::BatteryLow,
            Prompt::BatteryCritical,
            Prompt::NoContent,
            Prompt::WrongPassword,
        ];

        // When
        let sounds = prompts.map(Sound::for_prompt);

        // Then
        assert_eq!(
            sounds,
            [
                Sound::Startup,
                Sound::NoInternet,
                Sound::BatteryLow,
                Sound::BatteryCritical,
                Sound::NoStory,
                Sound::WrongPassword,
            ]
        );
    }
}
