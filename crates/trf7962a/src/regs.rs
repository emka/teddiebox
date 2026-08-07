//! TRF7962A registers and commands.
//!
//! **Unverified against silicon.** Taken from the TI TRF7962A datasheet
//! (SLOS743). Phase B step 10 is the first time these meet the real reader.

pub const CHIP_STATUS_CONTROL: u8 = 0x00;
pub const ISO_CONTROL: u8 = 0x01;
pub const TX_TIMER_HIGH: u8 = 0x08;
pub const TX_TIMER_LOW: u8 = 0x09;
pub const TX_PULSE_LENGTH: u8 = 0x0A;
pub const RX_NO_RESPONSE_WAIT: u8 = 0x0B;
/// Registers 0x0C and 0x0D are the IRQ status and mask.
///
/// Several community sources also list an RX wait time and a modulator/system
/// clock register at these addresses. **Confirm against SLOS743 before writing
/// to either**, and note that the init sequence deliberately avoids them: a
/// stray write to the IRQ mask would silently disable tag interrupts.
pub const IRQ_STATUS: u8 = 0x0C;
pub const IRQ_MASK: u8 = 0x0D;
pub const RX_SPECIAL_SETTINGS: u8 = 0x0F;
pub const FIFO_STATUS: u8 = 0x1C;
pub const TX_LENGTH_BYTE1: u8 = 0x1D;
pub const TX_LENGTH_BYTE2: u8 = 0x1E;
pub const FIFO: u8 = 0x1F;

/// Direct commands, sent with the command bit set.
pub mod cmd {
    pub const IDLE: u8 = 0x00;
    pub const SOFT_INIT: u8 = 0x03;
    pub const RESET_FIFO: u8 = 0x0F;
    pub const TRANSMIT_WITH_CRC: u8 = 0x11;
    pub const ENABLE_RX: u8 = 0x17;
}

/// ISO 15693, high bit rate, one subcarrier, 1-out-of-4 coding.
pub const ISO_CONTROL_15693_HIGH: u8 = 0x02;
/// RF output on, full power, 5 V operation.
pub const CHIP_STATUS_RF_ON: u8 = 0x21;
