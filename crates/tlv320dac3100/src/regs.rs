//! Register addresses, by page.
//!
//! **Unverified against silicon.** These values come from the TLV320DAC3100
//! datasheet (SLAS671C) and are checked only by the mocks in this crate.
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
    /// Read-only. Reports what actually powered up, as opposed to what was
    /// asked for: D7 left DAC, D5 HPL driver, D4 left class-D, D3 right DAC,
    /// D0 right class-D.
    pub const DAC_FLAGS: u8 = 0x25;
    pub const DAC_PROCESSING_BLOCK: u8 = 0x3C;
    pub const DAC_DATA_PATH: u8 = 0x3F;
    pub const DAC_MUTE_CTRL: u8 = 0x40;
    pub const DAC_LEFT_VOLUME: u8 = 0x41;
    pub const DAC_RIGHT_VOLUME: u8 = 0x42;
    /// Headset detection: D7 enables it, D6-D5 report what is plugged in.
    pub const HEADSET_DETECT: u8 = 0x43;
}

pub mod page1 {
    pub const HP_DRIVERS: u8 = 0x1F;
    pub const SPK_AMP: u8 = 0x20;
    /// Output driver pop-removal settings: D7 orders the power-down, D6-D3 the
    /// driver power-on time and D2-D1 the gain ramp step.
    pub const HP_POP_REMOVAL: u8 = 0x21;
    /// The datasheet calls this MICBIAS, but D7 is the device software
    /// power-down enable and that is the only bit this driver uses. Named for
    /// the datasheet so it can be found there.
    pub const MICBIAS: u8 = 0x2E;
    /// DAC_L and DAC_R output mixer routing.
    pub const OUTPUT_MIXER_ROUTING: u8 = 0x23;
    /// Analog volume controls, one per output driver.
    ///
    /// D7 routes the volume control to its driver and D6–D0 is the gain, whose
    /// reset value is –78 dB. Both halves matter: an unrouted driver is silent,
    /// and so is a routed one left at its reset gain.
    pub const HPL_ANALOG_VOLUME: u8 = 0x24;
    pub const HPR_ANALOG_VOLUME: u8 = 0x25;
    pub const SPK_ANALOG_VOLUME: u8 = 0x26;
    pub const HPL_DRIVER_GAIN: u8 = 0x28;
    pub const HPR_DRIVER_GAIN: u8 = 0x29;
    pub const SPK_DRIVER_GAIN: u8 = 0x2A;
}

pub mod page3 {
    /// The 1 MHz reference the headset-detection debounce counts on. D7
    /// selects the clock source — set for an external MCLK, which is its
    /// reset value and which this board does not wire — and D6-D0 divide it.
    pub const TIMER_CLOCK: u8 = 0x10;
}
