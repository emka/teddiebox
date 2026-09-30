//! The shared vocabulary of the reducer.

pub use teddiebox_board::Side;

/// Milliseconds since boot. Supplied by the caller; the core never reads a clock.
pub type Millis = u64;

/// The unique identifier of an ISO 15693 tag, as read from the figure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TagUid(pub [u8; 8]);

impl TagUid {
    /// The UID in the byte order used everywhere outside the reader: the
    /// card's `<8 hex>/<8 hex>` path, teddyCloud's ruid, and the `get`
    /// command's argument.
    ///
    /// The reader returns the UID least-significant byte first; everything
    /// else uses the reverse order.
    pub fn ruid(self) -> u64 {
        let mut bytes = self.0;
        bytes.reverse();
        u64::from_be_bytes(bytes)
    }
}

/// Where a story should resume.
///
/// An enum rather than a bare `u32`, so "nothing saved" is its own case and
/// not a magic zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Position {
    /// Nothing is remembered: play from the beginning.
    #[default]
    Start,
    /// The Ogg page index within the TAF file that the decoder had reached.
    Exact { page: u32 },
}

/// The two ears, named by size rather than by side.
///
/// A Toniebox has one large ear and one small one, which anyone can tell
/// apart. "Left" and "right" are ambiguous: the box's right is the left of
/// someone facing it.
///
/// The discriminants are explicit because the reducer indexes arrays by ear.
/// They follow the pin order in [`crate::input`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum Ear {
    /// GPIO20, the box's right.
    Larger = 0,
    /// GPIO21, the box's left.
    Smaller = 1,
}

/// Volume step. Zero is silent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Volume(pub u8);

/// Stock-equivalent number of steps above silence.
pub const MAX_VOLUME: u8 = 5;

/// Where the sound is going.
///
/// The headphone socket on this board does not switch the speaker off, so the
/// firmware chooses the output.
///
/// The discriminants are explicit because the reducer indexes one
/// [`crate::VolumeModel`] per output by this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum Output {
    Speaker = 0,
    Headphones = 1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedState {
    Off,
    Booting,
    Ready,
    Playing,
    Fetching,
    Charging,
    /// The pack is running low but the box still works. A warning, in orange.
    BatteryLow,
    /// The pack is nearly gone and the box is about to stop, in red.
    BatteryCritical,
    Error,
    /// The box is serving the setup page and will not play anything.
    ///
    /// Uses a colour no other state uses, so it is obvious the box is not
    /// going to play.
    Setup,
}

impl LedState {
    /// Every state, so a test can check the round trip below for all of them.
    pub const ALL: [LedState; 10] = [
        LedState::Off,
        LedState::Booting,
        LedState::Ready,
        LedState::Playing,
        LedState::Fetching,
        LedState::Charging,
        LedState::BatteryLow,
        LedState::BatteryCritical,
        LedState::Error,
        LedState::Setup,
    ];

    /// The byte that carries this state between tasks.
    ///
    /// The reducer runs in the media task and the LED belongs to the console
    /// loop, so the state is passed as one atomic byte. Each state has an
    /// explicit number rather than a cast, so reordering the enum cannot
    /// change what a byte means.
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
            LedState::Setup => 9,
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
            9 => Some(LedState::Setup),
            _ => None,
        }
    }
}

/// Spoken or tonal feedback the firmware renders from a bundled asset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prompt {
    Startup,
    NoContent,
    NoNetwork,
    /// The access point refused the passphrase on the card.
    ///
    /// Separate from [`Prompt::NoNetwork`] because the fix is different: this
    /// one means check the card, that one means check the router.
    WrongPassword,
    BatteryLow,
    /// The pack is nearly gone and the box is about to stop.
    ///
    /// Unlike [`Prompt::BatteryLow`], which is only a warning, this announces
    /// that the box is shutting down.
    BatteryCritical,
}

/// Why the box is powering off.
///
/// Both reasons power off the same way. The reason is only shown on the
/// console, so the log names the right cause.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerOffReason {
    /// The pack fell below the hard cutoff.
    PackEmpty,
    /// Nothing has used the box for `idle_timeout_ms`.
    Idle,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_position_with_nothing_saved_is_the_start() {
        // Given: nothing is saved

        // When
        let position = Position::default();

        // Then
        assert_eq!(position, Position::Start);
    }

    /// A real Tonie: the reader returns its UID as `E0040350503F2E1D`, and
    /// its story is stored at `CONTENT/1D2E3F50/500304E0`.
    #[test]
    fn a_real_tonie_uid_reverses_to_the_identifier_its_story_is_filed_under() {
        // Given
        let tag = TagUid([0xE0, 0x04, 0x03, 0x50, 0x50, 0x3F, 0x2E, 0x1D]);

        // When
        let ruid = tag.ruid();

        // Then
        assert_eq!(ruid, 0x1D2E_3F50_5003_04E0);
    }
}
