#![no_std]

//! Driver for the ST LIS3DH accelerometer.
//!
//! Enough of the part for bench step 5 and for M7: prove it is there, stream
//! axes, and run its click engine.
//!
//! Tap detection was going to live in `teddiebox-core` where a host test could
//! hold it. It does not, because the box polls this part every 200 ms and a
//! slap lasts a few: the part detects the click itself at its own rate and
//! latches it, which is the only reading that survives that poll interval.
//! What stays out of here is *policy* — which side of the box an axis means is
//! `teddiebox_core::board`'s business, and this crate still only moves bytes.

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
    /// Worth calling before anything else: this address is shared with the
    /// audio codec, which will acknowledge and answer nonsense.
    pub fn is_present(&mut self) -> Result<bool, Error<E>> {
        Ok(self.who_am_i()? == regs::DEVICE_ID)
    }

    /// Starts the device at 50 Hz with all three axes enabled.
    pub fn init(&mut self) -> Result<(), Error<E>> {
        self.i2c
            .write(self.address, &[regs::CTRL_REG1, regs::CTRL_REG1_50HZ_XYZ])
            .map_err(Error::Bus)
    }

    /// Reads X, Y and Z as raw signed counts.
    ///
    /// One burst with the auto-increment bit set, so the three axes come from
    /// the same sample rather than from three separate ones.
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
    /// The whole register as it was read. Carried because this datasheet does
    /// not pin the bit positions down, so the bench must be able to see what
    /// the part actually said rather than only what we made of it.
    pub raw: u8,
}

impl<I2C, E> Lis3dh<I2C>
where
    I2C: I2c<Error = E>,
{
    /// Takes the latched click, if there is one.
    ///
    /// Reading `CLICK_SRC` is what clears the latch, so a click is delivered
    /// exactly once however long it waited.
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
/// A set rather than one axis: which axis a slap lands on depends on how the
/// part is oriented in the box, and nothing has measured that. Calibration
/// enables all three and reads the answer off `Click::axis`.
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
    /// `CLICK_THS[6:0]`. One LSB is full scale / 128, so 15.625 mg at the
    /// +/-2 g the part defaults to and [`Lis3dh::init`] leaves in place.
    /// Values above 127 are clamped to 127, not masked.
    pub threshold: u8,
    /// `TIME_LIMIT[6:0]`, in ODR periods — 20 ms each at the 50 Hz `init`
    /// sets. **The unit is not stated in DocID17530 Rev 2**; it is ST's
    /// AN3308 that gives it as 1/ODR. Confirm at the bench before trusting it.
    pub time_limit: u8,
}

impl<I2C, E> Lis3dh<I2C>
where
    I2C: I2c<Error = E>,
{
    /// Sets the part detecting clicks and latching them.
    ///
    /// Call after [`init`](Self::init): the rate and the axis enables it
    /// writes are what the click engine runs on.
    ///
    /// `CLICK_CFG` — the per-axis enables — is written **last**, after
    /// `CLICK_THS` and `TIME_LIMIT`, and not first as the register map is
    /// laid out. `CLICK_THS` powers up at 0, its most sensitive setting, so a
    /// bus error partway through this call must never leave the axes armed
    /// against that default: on a child's toy that reads as random chapter
    /// skips from being carried across a room. Writing the axis enables last
    /// means a partial write leaves the click engine disarmed — silent —
    /// rather than armed and hypersensitive. Do not reorder this back to
    /// match the register map.
    pub fn enable_click(&mut self, cfg: ClickConfig) -> Result<(), Error<E>> {
        // Clamped, not masked: masking 200 would give 72 and quietly make the
        // box twice as sensitive as asked. Clamping fails towards a missed
        // slap, which is the safe direction for a thing that skips chapters.
        let ths = regs::CLICK_THS_LIR | cfg.threshold.min(regs::CLICK_THS_MAX);
        for write in [
            [regs::CTRL_REG2, regs::CTRL_REG2_HPCLICK],
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
