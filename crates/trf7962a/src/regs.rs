//! TRF7962A registers and commands.
//!
//! Addresses follow Table 6-1 of the TI datasheet (SLOS757C). **Unverified
//! against silicon**: Phase B step 10 is the first time these meet the real
//! reader.

pub const CHIP_STATUS_CONTROL: u8 = 0x00;
pub const ISO_CONTROL: u8 = 0x01;
/// The protocol sub-setting registers, 0x06 through 0x0B, are absent on purpose.
///
/// The reader presets all of them from the ISO Control write, so this driver
/// never has cause to address them, and 0x0F is the read-only RSSI register
/// rather than a writable setting. Naming them would only invite a write.
pub const IRQ_STATUS: u8 = 0x0C;
/// Collision position and interrupt mask — SLOS757C gives it both jobs.
///
/// The init sequence leaves it alone deliberately: a stray write here would
/// silently disable tag interrupts.
pub const IRQ_MASK: u8 = 0x0D;
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
