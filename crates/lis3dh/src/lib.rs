#![no_std]

//! Driver for the ST LIS3DH accelerometer.
//!
//! Enough of the part to check it is there, read the axes, and use its
//! click (tap) detection.
//!
//! The chip detects clicks itself and latches them, because the box only
//! polls it every 200 ms and a slap lasts a few milliseconds. Which side of
//! the box an axis means is decided in `teddiebox_board`; this crate only
//! moves bytes.

pub mod regs;

use embedded_hal::i2c::I2c;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error<E> {
    Bus(E),
}

pub struct Lis3dh<I2C> {
    i2c: I2C,
    address: u8,
}

impl<I2C, E> Lis3dh<I2C>
where
    I2C: I2c<Error = E>,
{
    /// `address` is [`regs::ADDRESS_SA0_LOW`] or [`regs::ADDRESS_SA0_HIGH`],
    /// selected on the board by the SDO/SA0 pin.
    pub const fn new(i2c: I2C, address: u8) -> Self {
        Self { i2c, address }
    }

    pub fn release(self) -> I2C {
        self.i2c
    }

    /// Reads the identity register.
    pub fn who_am_i(&mut self) -> Result<u8, Error<E>> {
        let mut buf = [0u8; 1];
        self.i2c
            .write_read(self.address, &[regs::WHO_AM_I], &mut buf)
            .map_err(Error::Bus)?;
        Ok(buf[0])
    }

    /// True when the identity register reads what a LIS3DH reads.
    ///
    /// Call this first: the address may be shared with the audio codec,
    /// which would also answer.
    pub fn is_present(&mut self) -> Result<bool, Error<E>> {
        Ok(self.who_am_i()? == regs::DEVICE_ID)
    }

    /// Starts the device at 400 Hz and +/-8 g, with all three axes enabled.
    pub fn init(&mut self) -> Result<(), Error<E>> {
        self.i2c
            .write(self.address, &[regs::CTRL_REG1, regs::CTRL_REG1_400HZ_XYZ])
            .map_err(Error::Bus)?;
        // Set full scale before any reads or thresholds: at the default
        // +/-2 g a slap clips and looks like normal handling. See
        // `CTRL_REG4_FS_8G`.
        self.i2c
            .write(self.address, &[regs::CTRL_REG4, regs::CTRL_REG4_FS_8G])
            .map_err(Error::Bus)
    }

    /// Reads X, Y and Z as raw signed counts.
    ///
    /// One burst read with auto-increment, so all three axes come from the
    /// same sample.
    pub fn acceleration(&mut self) -> Result<[i16; 3], Error<E>> {
        let mut buf = [0u8; 6];
        self.i2c
            .write_read(
                self.address,
                &[regs::OUT_X_L | regs::AUTO_INCREMENT],
                &mut buf,
            )
            .map_err(Error::Bus)?;

        Ok([
            i16::from_le_bytes([buf[0], buf[1]]),
            i16::from_le_bytes([buf[2], buf[3]]),
            i16::from_le_bytes([buf[4], buf[5]]),
        ])
    }
}

/// One axis of the part, as `CLICK_SRC` names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClickAxis {
    X,
    Y,
    Z,
}

/// A click the part detected and latched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Click {
    pub axis: ClickAxis,
    /// `CLICK_SRC`'s sign bit: which way the box was struck.
    pub negative: bool,
    /// The raw register value. Kept because the datasheet is unclear about
    /// the bit positions, so the raw value is useful for debugging.
    pub raw: u8,
}

impl<I2C, E> Lis3dh<I2C>
where
    I2C: I2c<Error = E>,
{
    /// Takes the latched click, if there is one.
    ///
    /// Reading `CLICK_SRC` clears the latch, so each click is returned once,
    /// however long it waited.
    pub fn take_click(&mut self) -> Result<Option<Click>, Error<E>> {
        let mut buf = [0u8; 1];
        self.i2c
            .write_read(self.address, &[regs::CLICK_SRC], &mut buf)
            .map_err(Error::Bus)?;
        let raw = buf[0];
        if raw & regs::CLICK_SRC_IA == 0 {
            return Ok(None);
        }
        let axis = if raw & regs::CLICK_SRC_X != 0 {
            ClickAxis::X
        } else if raw & regs::CLICK_SRC_Y != 0 {
            ClickAxis::Y
        } else if raw & regs::CLICK_SRC_Z != 0 {
            ClickAxis::Z
        } else {
            return Ok(None);
        };
        Ok(Some(Click {
            axis,
            negative: raw & regs::CLICK_SRC_SIGN != 0,
            raw,
        }))
    }
}

/// Which axes the click engine watches.
///
/// A set rather than one axis, so all three can be enabled and the axis read
/// from `Click::axis`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClickAxes {
    pub x: bool,
    pub y: bool,
    pub z: bool,
}

impl ClickAxes {
    pub const ALL: Self = Self {
        x: true,
        y: true,
        z: true,
    };

    const fn bits(self) -> u8 {
        let mut bits = 0;
        if self.x {
            bits |= regs::CLICK_CFG_XS;
        }
        if self.y {
            bits |= regs::CLICK_CFG_YS;
        }
        if self.z {
            bits |= regs::CLICK_CFG_ZS;
        }
        bits
    }
}

/// How the click engine is tuned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClickConfig {
    pub axes: ClickAxes,
    /// `CLICK_THS[6:0]`. One step is full scale / 128: about 62 mg at the
    /// +/-8 g that [`Lis3dh::init`] sets. Values above 127 are clamped to 127,
    /// not masked.
    pub threshold: u8,
    /// `TIME_LIMIT[6:0]`, in sample periods: 2.5 ms each at the 400 Hz that
    /// `init` sets. **The unit is not stated in DocID17530 Rev 2**; ST's
    /// AN3308 gives it as 1/ODR. Not verified on the hardware.
    pub time_limit: u8,
}

/// The threshold [`enable_click`](Lis3dh::enable_click) actually applies for
/// a requested value.
///
/// `CLICK_THS` is seven bits, so values above 127 are clamped, not masked
/// (masking 200 would give 72, making the box much more sensitive). Code that
/// reports the threshold should call this rather than print the raw request.
pub const fn clamped_threshold(threshold: u8) -> u8 {
    if threshold > regs::CLICK_THS_MAX {
        regs::CLICK_THS_MAX
    } else {
        threshold
    }
}

impl<I2C, E> Lis3dh<I2C>
where
    I2C: I2c<Error = E>,
{
    /// Sets the part detecting clicks and latching them.
    ///
    /// Call after [`init`](Self::init), which sets the sample rate and
    /// enables the axes.
    ///
    /// `CLICK_CFG` (the per-axis enables) is written **last**. `CLICK_THS`
    /// starts at 0, the most sensitive setting, so if a bus error interrupts
    /// this call, the axes must not already be enabled; otherwise carrying
    /// the box would skip chapters at random. Do not reorder these writes to
    /// match the register map.
    ///
    /// `REFERENCE` (0x26) is read and discarded after `CTRL_REG2`, before the
    /// axes are enabled. See the comment on that read.
    pub fn enable_click(&mut self, cfg: ClickConfig) -> Result<(), Error<E>> {
        let ths = regs::CLICK_THS_LIR | clamped_threshold(cfg.threshold);

        self.i2c
            .write(self.address, &[regs::CTRL_REG2, regs::CTRL_REG2_HPCLICK])
            .map_err(Error::Bus)?;

        // CTRL_REG2 selects HPM[1:0] = 00, "Normal mode, reset by reading
        // REFERENCE" (Table 34). Until REFERENCE is read, the filter output
        // still contains gravity's 1 g on Z as a step, which alone can cross
        // CLICK_THS and latch a false click. The value is discarded; the
        // read itself resets the filter. Not dead code.
        let mut reference = [0u8; 1];
        self.i2c
            .write_read(self.address, &[regs::REFERENCE], &mut reference)
            .map_err(Error::Bus)?;

        for write in [
            [regs::CLICK_THS, ths],
            [regs::TIME_LIMIT, cfg.time_limit.min(regs::TIME_LIMIT_MAX)],
            [regs::CLICK_CFG, cfg.axes.bits()],
        ] {
            self.i2c.write(self.address, &write).map_err(Error::Bus)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
