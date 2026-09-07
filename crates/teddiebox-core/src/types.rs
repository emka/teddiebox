//! The shared vocabulary of the reducer.

/// Milliseconds since boot. Supplied by the caller; the core never reads a clock.
pub type Millis = u64;

/// The unique identifier of an ISO 15693 tag, as read from the figure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TagUid(pub [u8; 8]);

/// A resume point, expressed as an Ogg page index within the TAF file.
///
/// Page granularity is deliberate: TAF pages are 4096 bytes, so at typical
/// Tonie bitrates one page is roughly a third of a second. That is precise
/// enough to resume a story, costs four bytes to persist, and is directly
/// seekable without decoding anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Position {
    pub page: u32,
}

/// The discriminants are explicit because the reducer indexes its
/// press-timestamp array by ear.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum Ear {
    Left = 0,
    Right = 1,
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
