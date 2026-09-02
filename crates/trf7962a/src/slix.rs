//! NXP ICODE SLIX privacy mode.
//!
//! Tonie figures ship with privacy mode enabled: the tag ignores inventory
//! entirely until it receives the correct privacy password. The unlock is a
//! two-step exchange — fetch a random number, then send the password XORed
//! with that number, twice over.
//!
//! **Unverified against silicon.** Command codes are from the NXP ICODE SLIX
//! datasheet; the password itself is a Toniebox-specific constant supplied by
//! the caller, not held here.

/// Request flags: one subcarrier, high data rate, not addressed.
///
/// ISO 15693-3 §7.3.1 puts the Address flag in bit 6, and a request that sets
/// it must carry the tag's eight-byte UID. During a privacy unlock the UID is
/// exactly what is not yet known — a locked tag will not answer inventory, so
/// there is nothing to address — which makes 0x02 the only flags byte these
/// two commands can carry. Nothing marks a request as manufacturer-custom:
/// that is the command code's range plus the IC manufacturer byte after it.
pub const FLAGS: u8 = 0x02;
/// NXP's IC manufacturer code.
pub const MFG_NXP: u8 = 0x04;

pub const CMD_GET_RANDOM_NUMBER: u8 = 0xB2;
pub const CMD_SET_PASSWORD: u8 = 0xB3;

/// Password identifier for the privacy password.
pub const PASSWORD_ID_PRIVACY: u8 = 0x04;

/// Builds the GET RANDOM NUMBER request.
pub fn get_random_number_request() -> [u8; 3] {
    [FLAGS, CMD_GET_RANDOM_NUMBER, MFG_NXP]
}

/// Builds the SET PASSWORD request.
///
/// The password is transmitted XORed with the random number repeated across
/// both halves, so the plaintext never crosses the air gap.
pub fn set_password_request(password: u32, random: u16) -> [u8; 8] {
    let xor_mask = (u32::from(random) << 16) | u32::from(random);
    let masked = (password ^ xor_mask).to_le_bytes();
    [
        FLAGS,
        CMD_SET_PASSWORD,
        MFG_NXP,
        PASSWORD_ID_PRIVACY,
        masked[0],
        masked[1],
        masked[2],
        masked[3],
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ISO 15693-3 §7.3.1: bit 6 of the request flags is the Address flag,
    /// and setting it promises an eight-byte UID that these three bytes do
    /// not carry. A tag reading a malformed addressed request stays silent,
    /// which at a bench is indistinguishable from an empty plate.
    #[test]
    fn the_random_number_request_is_not_addressed_to_a_uid() {
        assert_eq!(
            get_random_number_request(),
            [0x02, 0xB2, 0x04],
            "the UID is what an unlock does not yet know"
        );
    }

    #[test]
    fn the_password_request_is_not_addressed_to_a_uid() {
        assert_eq!(&set_password_request(0, 0)[..3], &[0x02, 0xB3, 0x04]);
    }

    #[test]
    fn the_password_is_masked_with_the_random_number_in_both_halves() {
        let request = set_password_request(0x0000_0000, 0xABCD);
        // Zero XOR mask is the mask itself, little-endian.
        assert_eq!(&request[4..], &[0xCD, 0xAB, 0xCD, 0xAB]);
    }

    #[test]
    fn a_zero_random_number_transmits_the_password_unchanged() {
        let request = set_password_request(0x1122_3344, 0x0000);
        assert_eq!(&request[4..], &0x1122_3344u32.to_le_bytes());
    }

    #[test]
    fn the_request_carries_the_privacy_password_identifier() {
        let request = set_password_request(1, 1);
        assert_eq!(request[3], PASSWORD_ID_PRIVACY);
    }
}
