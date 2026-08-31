//! What lives on the I2C bus, and at which address.
//!
//! A bus scan prints numbers; this turns them into names, so bench step 4 can
//! say whether the bus holds what the hardware inventory claims.

/// SDA, shared by the codec and the accelerometer.
pub const SDA: u8 = 5;
/// SCL, likewise.
pub const SCL: u8 = 6;

/// What is expected to answer, per the hardware inventory.
///
/// **`0x18` is ambiguous**: it is the TLV320DAC3100's only address and one of
/// the LIS3DH's two, selected by its SDO pin. If exactly one device answers at
/// `0x18` and nothing answers at `0x19`, the bus cannot tell us which part it
/// is — which matters, because the codec and the accelerometer are the two
/// parts identified from a wiki pinout rather than from their markings.
pub const fn describe(address: u8) -> Option<&'static str> {
    match address {
        0x18 => Some("TLV320DAC3100, or LIS3DH with SDO low"),
        0x19 => Some("LIS3DH with SDO high"),
        _ => None,
    }
}

/// The lowest and highest addresses worth probing.
///
/// Below 0x08 and above 0x77 are reserved by the I2C specification and a
/// device answering there would be a bus fault, not a discovery.
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

    /// Probing a reserved address is a bus fault waiting to happen.
    #[test]
    fn the_scan_range_excludes_the_reserved_addresses() {
        assert_eq!(FIRST_ADDRESS, 0x08);
        assert_eq!(LAST_ADDRESS, 0x77);
    }
}
