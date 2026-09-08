//! The shared vocabulary of the reducer.

/// Milliseconds since boot. Supplied by the caller; the core never reads a clock.
pub type Millis = u64;

/// The unique identifier of an ISO 15693 tag, as read from the figure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TagUid(pub [u8; 8]);

impl TagUid {
    /// The same identifier in the byte order everything outside the reader
    /// uses: the card's `<8 hex>/<8 hex>` path, teddyCloud's ruid, and the
    /// `get` command's argument.
    ///
    /// The reader hands the UID over least-significant byte first and every
    /// other party reads it the other way round, so one of the two has to
    /// reverse it. Doing it here means callers name the conversion instead of
    /// open-coding a `reverse()` each time they need a story's identity.
    pub fn ruid(self) -> u64 {
        let mut bytes = self.0;
        bytes.reverse();
        u64::from_be_bytes(bytes)
    }
}

/// A resume point, expressed as an Ogg page index within the TAF file.
///
/// Where a story should resume.
///
/// An enum rather than a bare `u32` so that "nothing is remembered" is a state
/// the type carries rather than a magic zero every caller has to remember to
/// check — and so a coarser kind can be added later without every reader
/// silently treating it as a page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Position {
    /// Nothing is remembered: play from the beginning.
    #[default]
    Start,
    /// The exact container page the decoder had reached.
    ///
    /// A chapter variant existed here briefly, for a coarse position written
    /// at every chapter boundary. It earned nothing: writing the exact page
    /// costs the same bytes, and writing it when the figure is lifted costs
    /// one write instead of one per chapter.
    Exact { page: u32 },
}

/// The two ears, named by size rather than by side.
///
/// A Toniebox has one large ear and one small one, and which is which is the
/// only thing a person can tell without being told. "Left" and "right" are
/// worse than useless here: they are the *box's* left and right, so an
/// instruction to press the right ear gets the other ear pressed about half
/// the time — which it did, at the bench, on 2026-09-08.
///
/// The discriminants are explicit because the reducer indexes its
/// press-timestamp array by ear, and they follow the pin order in
/// [`crate::input`] so the two cannot drift apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum Ear {
    /// GPIO20, the box's left.
    Larger = 0,
    /// GPIO21, the box's right.
    Smaller = 1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeekDir {
    Forward,
    Backward,
}

/// Volume step. Zero is silent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Volume(pub u8);

/// Stock-equivalent number of steps above silence.
pub const MAX_VOLUME: u8 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedState {
    Off,
    Booting,
    Ready,
    Playing,
    Fetching,
    Charging,
    /// The pack is running out but the box still works — a warning, in orange.
    BatteryLow,
    /// The pack is nearly gone and the box is about to stop, in red.
    BatteryCritical,
    Error,
}

impl LedState {
    /// Every state, so a test can prove the round trip below covers them all
    /// rather than the handful somebody remembered.
    pub const ALL: [LedState; 9] = [
        LedState::Off,
        LedState::Booting,
        LedState::Ready,
        LedState::Playing,
        LedState::Fetching,
        LedState::Charging,
        LedState::BatteryLow,
        LedState::BatteryCritical,
        LedState::Error,
    ];

    /// The byte that carries this state between tasks.
    ///
    /// The reducer runs in the media task and the LED is owned by the console
    /// loop, so what passes between them is one atomic. Written as an explicit
    /// number per state rather than a cast, because a reordering of the enum
    /// would otherwise silently change what a stored byte means.
    pub const fn code(self) -> u8 {
        match self {
            LedState::Off => 0,
            LedState::Booting => 1,
            LedState::Ready => 2,
            LedState::Playing => 3,
            LedState::Fetching => 4,
            LedState::Charging => 5,
            LedState::BatteryLow => 6,
            LedState::BatteryCritical => 7,
            LedState::Error => 8,
        }
    }

    /// The state a byte names, or `None` if it names none.
    pub const fn from_code(code: u8) -> Option<LedState> {
        match code {
            0 => Some(LedState::Off),
            1 => Some(LedState::Booting),
            2 => Some(LedState::Ready),
            3 => Some(LedState::Playing),
            4 => Some(LedState::Fetching),
            5 => Some(LedState::Charging),
            6 => Some(LedState::BatteryLow),
            7 => Some(LedState::BatteryCritical),
            8 => Some(LedState::Error),
            _ => None,
        }
    }
}

/// Spoken or tonal feedback the firmware renders from a bundled asset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prompt {
    Startup,
    Shutdown,
    NoContent,
    NoNetwork,
    BatteryLow,
    VolumeLimit,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two tiers carry different things — an exact page in RAM, a chapter
    /// on the card — and a `page` that sometimes means a chapter is the kind
    /// of lie that costs an evening.
    #[test]
    fn a_position_with_nothing_saved_is_the_start() {
        assert_eq!(Position::default(), Position::Start);
    }

    /// A real Tonie off this project's own card: the reader hands its UID over
    /// as `E0040350503F2E1D`, and the story sits at `CONTENT/1D2E3F50/500304E0`.
    /// A synthetic vector would pass by construction; this one was observed.
    #[test]
    fn a_real_tonie_uid_reverses_to_the_identifier_its_story_is_filed_under() {
        let tag = TagUid([0xE0, 0x04, 0x03, 0x50, 0x50, 0x3F, 0x2E, 0x1D]);
        assert_eq!(tag.ruid(), 0x1D2E_3F50_5003_04E0);
    }
}
