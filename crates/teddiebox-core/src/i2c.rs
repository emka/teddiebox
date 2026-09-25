//! What lives on the I2C bus, and at which address.
//!
//! Turns I2C bus scan addresses into device names, to check that the expected
//! devices are on the bus.

/// SDA, shared by the codec and the accelerometer.
pub const SDA: u8 = 5;
/// SCL, likewise.
pub const SCL: u8 = 6;

/// Which device is expected at an address.
///
/// **`0x18` is ambiguous**: it is the TLV320DAC3100's only address and one of
/// the LIS3DH's two (chosen by its SDO pin). If one device answers at `0x18`
/// and none at `0x19`, the scan cannot tell which part it is.
pub const fn describe(address: u8) -> Option<&'static str> {
    match address {
        0x18 => Some("TLV320DAC3100, or LIS3DH with SDO low"),
        0x19 => Some("LIS3DH with SDO high"),
        _ => None,
    }
}

/// The lowest and highest addresses worth probing.
///
/// Addresses below 0x08 and above 0x77 are reserved by the I2C specification.
pub const FIRST_ADDRESS: u8 = 0x08;
pub const LAST_ADDRESS: u8 = 0x77;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shared_address_names_both_candidates() {
        let at_18 = describe(0x18).expect("something is expected at 0x18");
        assert!(at_18.contains("TLV320DAC3100"));
        assert!(at_18.contains("LIS3DH"));
    }

    #[test]
    fn the_second_accelerometer_address_is_unambiguous() {
        assert_eq!(describe(0x19), Some("LIS3DH with SDO high"));
    }

    #[test]
    fn an_unexpected_address_has_no_name() {
        assert_eq!(describe(0x50), None);
        assert_eq!(describe(0x00), None);
    }

    /// Reserved addresses are not probed.
    #[test]
    fn the_scan_range_excludes_the_reserved_addresses() {
        assert_eq!(FIRST_ADDRESS, 0x08);
        assert_eq!(LAST_ADDRESS, 0x77);
    }
}
