//! LIS3DH registers.
//!
//! Addresses and values from the ST datasheet (DocID17530 Rev 2). **Unverified
//! against silicon**: bench step 5 is the first time these meet the real part,
//! and the part itself is identified from a wiki pinout rather than from its
//! markings.

/// Identity register. Reads [`DEVICE_ID`] on a real LIS3DH.
pub const WHO_AM_I: u8 = 0x0F;
/// `00110011` per Table 24.
pub const DEVICE_ID: u8 = 0x33;

/// Data rate, low-power enable, and the three axis enables.
pub const CTRL_REG1: u8 = 0x20;

/// First of the six output bytes, X low through Z high.
pub const OUT_X_L: u8 = 0x28;

/// Set in a sub-address to make the device auto-increment through a burst.
///
/// Without it every byte of a six-byte read comes from the same register,
/// which yields a plausible-looking vector that never changes on two axes.
pub const AUTO_INCREMENT: u8 = 0x80;

/// 50 Hz, normal resolution, all three axes enabled.
///
/// `0100` selects 50 Hz (Table 31), LPen clear keeps normal mode, and the low
/// three bits enable Z, Y and X.
pub const CTRL_REG1_50HZ_XYZ: u8 = 0x47;

/// SA0 tied low. Shared with the TLV320DAC3100, which has no other address.
pub const ADDRESS_SA0_LOW: u8 = 0x18;
/// SA0 tied high.
pub const ADDRESS_SA0_HIGH: u8 = 0x19;

/// Click source. Reading it clears a latched click.
pub const CLICK_SRC: u8 = 0x39;

/// `CLICK_SRC`: one or more click interrupts have been generated.
pub const CLICK_SRC_IA: u8 = 0x40;
/// `CLICK_SRC`: 0 positive detection, 1 negative detection.
pub const CLICK_SRC_SIGN: u8 = 0x08;
pub const CLICK_SRC_Z: u8 = 0x04;
pub const CLICK_SRC_Y: u8 = 0x02;
pub const CLICK_SRC_X: u8 = 0x01;

/// High-pass filter configuration.
pub const CTRL_REG2: u8 = 0x21;
/// `CTRL_REG2`: high-pass filter enabled for the CLICK function.
pub const CTRL_REG2_HPCLICK: u8 = 0x04;

/// Click interrupt enables, per axis.
pub const CLICK_CFG: u8 = 0x38;
pub const CLICK_CFG_ZS: u8 = 0x10;
pub const CLICK_CFG_YS: u8 = 0x04;
pub const CLICK_CFG_XS: u8 = 0x01;

/// Click threshold, with the latch bit at the top.
pub const CLICK_THS: u8 = 0x3A;
/// `CLICK_THS`: hold the interrupt until `CLICK_SRC` is read.
pub const CLICK_THS_LIR: u8 = 0x80;
/// `CLICK_THS`: the threshold occupies the low seven bits.
pub const CLICK_THS_MAX: u8 = 0x7F;

/// How long the acceleration may stay over the threshold and still be a click.
pub const TIME_LIMIT: u8 = 0x3B;
/// `TIME_LIMIT` is seven bits too. Its own constant: the same value for a
/// different field is a coincidence, not a shared fact.
pub const TIME_LIMIT_MAX: u8 = 0x7F;
