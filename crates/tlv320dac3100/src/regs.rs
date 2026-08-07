//! Register addresses, by page.
//!
//! **Unverified against silicon.** These values come from the TLV320DAC3100
//! datasheet (SLAS667) and are checked only by the mocks in this crate.
//! Phase B step 6 is the first time they meet the real codec.

/// Register 0 on every page selects the active page.
pub const REG_PAGE_SELECT: u8 = 0x00;

pub mod page0 {
    pub const SOFTWARE_RESET: u8 = 0x01;
    pub const CLOCK_GEN_MUX: u8 = 0x04;
    pub const PLL_P_R: u8 = 0x05;
    pub const PLL_J: u8 = 0x06;
    pub const PLL_D_MSB: u8 = 0x07;
    pub const PLL_D_LSB: u8 = 0x08;
    pub const NDAC: u8 = 0x0B;
    pub const MDAC: u8 = 0x0C;
    pub const DOSR_MSB: u8 = 0x0D;
    pub const DOSR_LSB: u8 = 0x0E;
    pub const CODEC_IF_CTRL1: u8 = 0x1B;
    pub const DAC_PROCESSING_BLOCK: u8 = 0x3C;
    pub const DAC_DATA_PATH: u8 = 0x3F;
    pub const DAC_MUTE_CTRL: u8 = 0x40;
    pub const DAC_LEFT_VOLUME: u8 = 0x41;
    pub const DAC_RIGHT_VOLUME: u8 = 0x42;
}

pub mod page1 {
    pub const HP_DRIVERS: u8 = 0x1F;
    pub const SPK_AMP: u8 = 0x20;
    pub const HP_OUT_ROUTING: u8 = 0x23;
    pub const SPK_OUT_ROUTING: u8 = 0x24;
    pub const HPL_DRIVER_GAIN: u8 = 0x28;
    pub const HPR_DRIVER_GAIN: u8 = 0x29;
    pub const SPK_DRIVER_GAIN: u8 = 0x2A;
    pub const HP_DETECT: u8 = 0x2E;
}
