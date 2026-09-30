extern crate std;
use std::vec;

use super::*;
use embedded_hal_mock::eh1::i2c::{Mock as I2cMock, Transaction};

const ADDR: u8 = regs::ADDRESS_SA0_LOW;

/// Every byte is written by hand rather than taken from `regs`, so the tests
/// can disagree with the code.
#[test]
fn the_identity_register_is_read_from_0x0f() {
    // Given
    let expected = [Transaction::write_read(ADDR, vec![0x0F], vec![0x33])];
    let mut dev = Lis3dh::new(I2cMock::new(&expected), ADDR);

    // When
    let id = dev.who_am_i().unwrap();

    // Then
    assert_eq!(id, 0x33);
    dev.release().done();
}

#[test]
fn a_foreign_device_id_is_not_a_lis3dh() {
    // Given
    let expected = [Transaction::write_read(ADDR, vec![0x0F], vec![0x41])];
    let mut dev = Lis3dh::new(I2cMock::new(&expected), ADDR);

    // When
    let present = dev.is_present().unwrap();

    // Then
    assert!(!present);
    dev.release().done();
}

#[test]
fn initialisation_writes_the_rate_and_the_axis_enables() {
    // Given: a bus that expects exactly these writes
    let expected = [
        Transaction::write(ADDR, vec![0x20, 0x77]),
        Transaction::write(ADDR, vec![0x23, 0x20]),
    ];
    let mut dev = Lis3dh::new(I2cMock::new(&expected), ADDR);

    // When
    let initialised = dev.init();

    // Then
    assert!(initialised.is_ok());
    dev.release().done();
}

/// The burst must set the auto-increment bit, or the device returns the same
/// register six times.
#[test]
fn a_burst_read_sets_the_auto_increment_bit() {
    // Given
    let expected = [Transaction::write_read(
        ADDR,
        vec![0xA8],
        vec![0x00, 0x01, 0x00, 0xFF, 0x00, 0x40],
    )];
    let mut dev = Lis3dh::new(I2cMock::new(&expected), ADDR);

    // When
    let axes = dev.acceleration().unwrap();

    // Then
    assert_eq!(axes, [0x0100, -0x0100, 0x4000]);
    dev.release().done();
}

/// Little-endian and signed: a negative value must not read as a large
/// positive one.
#[test]
fn a_negative_axis_stays_negative() {
    // Given
    let expected = [Transaction::write_read(
        ADDR,
        vec![0xA8],
        vec![0x00, 0x80, 0x00, 0x00, 0x00, 0x00],
    )];
    let mut dev = Lis3dh::new(I2cMock::new(&expected), ADDR);

    // When
    let axes = dev.acceleration().unwrap();

    // Then
    assert_eq!(axes[0], i16::MIN);
    dev.release().done();
}

/// `CLICK_SRC`'s bit positions are ambiguous in DocID17530 Rev 2: Table 71
/// marks CLICK_CFG's unused bits with `--`, but Table 73 lists seven names
/// for CLICK_SRC with no padding. These bytes follow the driver's reading;
/// `Click::raw` exposes the raw register to check it.
#[test]
fn a_click_on_x_is_reported_with_its_axis() {
    // Given
    let expected = [Transaction::write_read(ADDR, vec![0x39], vec![0x41])];
    let mut dev = Lis3dh::new(I2cMock::new(&expected), ADDR);

    // When
    let click = dev.take_click().unwrap().expect("IA was set");

    // Then
    assert_eq!(click.axis, ClickAxis::X);
    assert!(!click.negative);
    assert_eq!(click.raw, 0x41);
    dev.release().done();
}

#[test]
fn the_sign_bit_says_which_way_the_box_was_struck() {
    // Given
    let expected = [Transaction::write_read(ADDR, vec![0x39], vec![0x4A])];
    let mut dev = Lis3dh::new(I2cMock::new(&expected), ADDR);

    // When
    let click = dev.take_click().unwrap().expect("IA was set");

    // Then
    assert_eq!(click.axis, ClickAxis::Y);
    assert!(click.negative);
    dev.release().done();
}

#[test]
fn no_interrupt_active_is_not_a_click() {
    // Given
    let expected = [Transaction::write_read(ADDR, vec![0x39], vec![0x00])];
    let mut dev = Lis3dh::new(I2cMock::new(&expected), ADDR);

    // When
    let click = dev.take_click().unwrap();

    // Then
    assert_eq!(click, None);
    dev.release().done();
}

/// An interrupt with no axis bit is not understood, so it is discarded
/// rather than guessed.
#[test]
fn an_interrupt_with_no_axis_is_discarded() {
    // Given
    let expected = [Transaction::write_read(ADDR, vec![0x39], vec![0x40])];
    let mut dev = Lis3dh::new(I2cMock::new(&expected), ADDR);

    // When
    let click = dev.take_click().unwrap();

    // Then
    assert_eq!(click, None);
    dev.release().done();
}

/// Byte for byte, by hand. 0x04 is HPCLICK in CTRL_REG2: without the
/// high-pass filter, gravity's 1 g on Z would affect click detection. The
/// read of 0x26 (REFERENCE) resets that filter, so it comes before the axes
/// are enabled; the returned byte is ignored. 0xAD is LIR_Click plus a
/// threshold of 45. 0x15 is ZS|YS|XS (single click on all three axes, no
/// double click), written last so a bus error earlier cannot leave the axes
/// enabled with CLICK_THS at its power-on value of 0.
#[test]
fn enabling_click_writes_the_filter_the_axes_the_threshold_and_the_limit() {
    // Given: a bus that expects exactly these transactions
    let expected = [
        Transaction::write(ADDR, vec![0x21, 0x04]),
        Transaction::write_read(ADDR, vec![0x26], vec![0x00]),
        Transaction::write(ADDR, vec![0x3A, 0xAD]),
        Transaction::write(ADDR, vec![0x3B, 0x03]),
        Transaction::write(ADDR, vec![0x38, 0x15]),
    ];
    let mut dev = Lis3dh::new(I2cMock::new(&expected), ADDR);

    // When
    let enabled = dev.enable_click(ClickConfig {
        axes: ClickAxes::ALL,
        threshold: 45,
        time_limit: 3,
    });

    // Then
    assert!(enabled.is_ok());
    dev.release().done();
}

#[test]
fn a_single_axis_enables_only_that_axis() {
    // Given: a bus that expects exactly these transactions
    let expected = [
        Transaction::write(ADDR, vec![0x21, 0x04]),
        Transaction::write_read(ADDR, vec![0x26], vec![0x00]),
        Transaction::write(ADDR, vec![0x3A, 0x81]),
        Transaction::write(ADDR, vec![0x3B, 0x00]),
        Transaction::write(ADDR, vec![0x38, 0x04]),
    ];
    let mut dev = Lis3dh::new(I2cMock::new(&expected), ADDR);

    // When
    let enabled = dev.enable_click(ClickConfig {
        axes: ClickAxes {
            x: false,
            y: true,
            z: false,
        },
        threshold: 1,
        time_limit: 0,
    });

    // Then
    assert!(enabled.is_ok());
    dev.release().done();
}

/// The threshold is seven bits. Masking 200 would give 72, making the box
/// more than twice as sensitive. Clamping errs towards missing a slap rather
/// than inventing one.
#[test]
fn an_oversized_threshold_clamps_to_the_least_sensitive_setting() {
    // Given: a bus that expects exactly these transactions
    let expected = [
        Transaction::write(ADDR, vec![0x21, 0x04]),
        Transaction::write_read(ADDR, vec![0x26], vec![0x00]),
        Transaction::write(ADDR, vec![0x3A, 0xFF]),
        Transaction::write(ADDR, vec![0x3B, 0x00]),
        Transaction::write(ADDR, vec![0x38, 0x15]),
    ];
    let mut dev = Lis3dh::new(I2cMock::new(&expected), ADDR);

    // When
    let enabled = dev.enable_click(ClickConfig {
        axes: ClickAxes::ALL,
        threshold: 200,
        time_limit: 0,
    });

    // Then
    assert!(enabled.is_ok());
    dev.release().done();
}

/// `clamped_threshold` is used by `enable_click` and by the console to
/// report the value actually applied. 127 is the seven-bit maximum.
#[test]
fn clamped_threshold_passes_in_range_values_through() {
    // Given
    let in_range = 45;

    // When
    let applied = clamped_threshold(in_range);

    // Then
    assert_eq!(applied, 45);
}

#[test]
fn clamped_threshold_ceilings_at_127() {
    // Given
    let too_high = 255;

    // When
    let applied = clamped_threshold(too_high);

    // Then
    assert_eq!(applied, 127);
}
