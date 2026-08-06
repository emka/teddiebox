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
    LowBattery,
    Error,
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
