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

#[cfg(test)]
mod tests;
