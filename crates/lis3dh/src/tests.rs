extern crate std;
use std::vec;

use super::*;
use embedded_hal_mock::eh1::i2c::{Mock as I2cMock, Transaction};

const ADDR: u8 = regs::ADDRESS_SA0_LOW;

/// Every byte written out by hand rather than taken from `regs`: a test that
/// reads the same constants as the code cannot disagree with it, which is how
/// a shifted register map ships green.
#[test]
fn the_identity_register_is_read_from_0x0f() {
    let expected = [Transaction::write_read(ADDR, vec![0x0F], vec![0x33])];
    let mut dev = Lis3dh::new(I2cMock::new(&expected), ADDR);
    assert_eq!(dev.who_am_i().unwrap(), 0x33);
    dev.release().done();
}

#[test]
fn a_foreign_device_id_is_not_a_lis3dh() {
    let expected = [Transaction::write_read(ADDR, vec![0x0F], vec![0x41])];
    let mut dev = Lis3dh::new(I2cMock::new(&expected), ADDR);
    assert!(!dev.is_present().unwrap());
    dev.release().done();
}

#[test]
fn initialisation_writes_the_rate_and_the_axis_enables() {
    let expected = [Transaction::write(ADDR, vec![0x20, 0x47])];
    let mut dev = Lis3dh::new(I2cMock::new(&expected), ADDR);
    dev.init().unwrap();
    dev.release().done();
}

/// The burst must set the auto-increment bit. Without it the device returns
/// the same register six times, which reads as a vector that never moves on
/// two of its three axes.
#[test]
fn a_burst_read_sets_the_auto_increment_bit() {
    let expected = [Transaction::write_read(
        ADDR,
        vec![0xA8],
        vec![0x00, 0x01, 0x00, 0xFF, 0x00, 0x40],
    )];
    let mut dev = Lis3dh::new(I2cMock::new(&expected), ADDR);
    assert_eq!(dev.acceleration().unwrap(), [0x0100, -0x0100, 0x4000]);
    dev.release().done();
}

/// Little endian, low byte first, and signed: a tilt one way must not read as
/// a large positive number.
#[test]
fn a_negative_axis_stays_negative() {
    let expected = [Transaction::write_read(
        ADDR,
        vec![0xA8],
        vec![0x00, 0x80, 0x00, 0x00, 0x00, 0x00],
    )];
    let mut dev = Lis3dh::new(I2cMock::new(&expected), ADDR);
    assert_eq!(dev.acceleration().unwrap()[0], i16::MIN);
    dev.release().done();
}
