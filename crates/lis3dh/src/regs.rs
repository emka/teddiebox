//! LIS3DH registers.
//!
//! Addresses and values from the ST datasheet (DocID17530 Rev 2). The part
//! was identified from a wiki pinout, not from its markings.

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
/// Without it, every byte of a six-byte read comes from the same register.
pub const AUTO_INCREMENT: u8 = 0x80;

/// 50 Hz, normal resolution, all three axes enabled.
///
/// `0100` selects 50 Hz (Table 31), LPen clear keeps normal mode, and the low
/// three bits enable Z, Y and X.
pub const CTRL_REG1_50HZ_XYZ: u8 = 0x47;

/// 400 Hz, normal mode, all three axes enabled.
///
/// Click detection runs at the sample rate, which decides how finely
/// `TIME_LIMIT` can separate a slap from the box rocking afterwards: 2.5 ms
/// per sample here, against 20 ms at 50 Hz. At 50 Hz a slap and its recoil
/// could not be told apart, and the detected direction was random.
pub const CTRL_REG1_400HZ_XYZ: u8 = 0x77;

/// Full-scale selection and more. `FS[1:0]` are bits 5:4.
pub const CTRL_REG4: u8 = 0x23;

/// `FS[1:0] = 10`, +/-8 g.
///
/// Not the +/-2 g default. At +/-2 g a slap saturates at about 2000 mg, while
/// normal handling (placing a figure, putting the box down) reaches 1500 mg,
/// so no threshold separates them. At +/-8 g a slap shows its real size and
/// is well above handling.
///
/// The cost is resolution: one `CLICK_THS` step is 62 mg here, against 16 mg
/// at +/-2 g.
pub const CTRL_REG4_FS_8G: u8 = 0x20;

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

/// High-pass filter reference. Reading it resets the filter (Table 34,
/// `HPM[1:0] = 00`: "Normal mode, reset by reading REFERENCE").
pub const REFERENCE: u8 = 0x26;

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
/// `TIME_LIMIT` is also seven bits. A separate constant, because the two
/// fields only happen to have the same limit.
pub const TIME_LIMIT_MAX: u8 = 0x7F;
