#![no_std]

//! Driver for the ST LIS3DH accelerometer.
//!
//! Enough of the part for bench step 5: prove it is there, then stream axes.
//! Tap and tilt detection come later, and belong in `teddiebox-core` where a
//! host test can hold them — this crate only moves bytes.

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

#[cfg(test)]
mod tests;
