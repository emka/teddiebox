//! TRF7962A registers and commands.
//!
//! Addresses follow Table 6-1 of the TI datasheet (SLOS757C).

pub const CHIP_STATUS_CONTROL: u8 = 0x00;
pub const ISO_CONTROL: u8 = 0x01;
/// The protocol sub-setting registers, 0x06 through 0x0B, are left out on
/// purpose: the reader sets them from the ISO Control write, and naming them
/// would only invite a write. (0x0F is the read-only RSSI register.)
pub const IRQ_STATUS: u8 = 0x0C;
/// Collision position and interrupt mask; SLOS757C gives it both jobs.
///
/// Not written at start-up: a wrong write would silently disable tag
/// interrupts.
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
/// RF output on, full power, 3-V operation.
///
/// B0 is `vrs5_3` and selects the supply range: 1 is 5-V operation, 0 is 3-V
/// (SLOS757G Table 6-16). §6.4: "if the supply voltage is below 4.3 V, the
/// 3-V configuration should be used". The reader shares power gate 47 with
/// the SD card and runs at 3.3 V. The setting changes how the supply
/// regulators are configured. Note the reset default, `0x21`, selects 5-V
/// operation.
pub const CHIP_STATUS_RF_ON: u8 = 0x20;

/// B5, `rf_on`: transmitter on, receivers on.
pub const RF_ON_BIT: u8 = 0x20;

/// The same word with the field taken away.
///
/// Derived from `CHIP_STATUS_RF_ON`, so both use the same supply setting.
pub const CHIP_STATUS_RF_OFF: u8 = CHIP_STATUS_RF_ON & !RF_ON_BIT;
