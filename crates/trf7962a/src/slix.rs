//! NXP ICODE privacy mode.
//!
//! Tonie figures come with privacy mode on: the tag ignores inventory until
//! it receives the correct privacy password. Unlocking takes two steps: get
//! a random number, then send the password XORed with that number.
//!
//! Tested on a Tonie figure and on a SLIX-L; both answered inventory after
//! being unlocked this way.
//!
//! The module is named SLIX, but **plain ICODE SLIX has no privacy mode**;
//! only SLIX-L and SLIX2 do (SL2S5002 §1.3). **SL2S2602, the SLIX2 data
//! sheet, is the reference for everything here**; it is the only public one
//! in the family with the full command table.
//!
//! Table 13 gives the password identifiers: `01h` read, `02h` write, **`04h`
//! privacy**, `08h` destroy, `10h` EAS/AFI. §9.5.3.2: "the SET PASSWORD
//! command can only be executed in Addressed or Selected mode **except for
//! the Privacy password**", so the unaddressed request below is allowed; a
//! locked tag would not reveal the UID an addressed request needs.
//!
//! **A wrong password gets no answer, and the tag then ignores every command
//! until the field is turned off** (SL2S2602 §9.5.3.2). So the field must be
//! reset before trying another password.
//!
//! The Toniebox's password is passed in by the caller, not stored here.

/// Request flags: one subcarrier, high data rate, not addressed.
///
/// ISO 15693-3 §7.3.1 puts the Address flag in bit 6, and an addressed
/// request must include the tag's UID. A locked tag does not reveal its UID,
/// so these commands must be unaddressed. A custom command is marked by its
/// command code and the manufacturer byte, not by a flag.
pub const FLAGS: u8 = 0x02;
/// NXP's IC manufacturer code.
pub const MFG_NXP: u8 = 0x04;

pub const CMD_GET_RANDOM_NUMBER: u8 = 0xB2;
pub const CMD_SET_PASSWORD: u8 = 0xB3;
/// SL2S2602 §9.5.3.9. **One bit away from DESTROY at `0xB9`**; see
/// `enable_privacy_request` for why it is never sent addressed.
pub const CMD_ENABLE_PRIVACY: u8 = 0xBA;

/// Password identifier for the privacy password.
pub const PASSWORD_ID_PRIVACY: u8 = 0x04;

/// NXP's factory default privacy password.
///
/// SL2S2602 §9.2.3: "all password bytes are 0Fh for the Privacy and Destroy
/// passwords" on delivery. A new, unused tag still has this password.
/// Confirmed on a SLIX-L that refused the Toniebox password but accepted this
/// one.
///
/// A published default, not a secret, so it can be in the code.
///
/// **The Destroy password has the same default**, and DESTROY (`B9h`) is one
/// bit away from ENABLE PRIVACY (`BAh`). On a new tag, a one-bit mistake in
/// the command code would permanently destroy it. DESTROY is not implemented
/// and must not be added without great care.
pub const VENDOR_DEFAULT_PASSWORD: u32 = 0x0F0F_0F0F;

/// Builds the GET RANDOM NUMBER request.
pub fn get_random_number_request() -> [u8; 3] {
    [FLAGS, CMD_GET_RANDOM_NUMBER, MFG_NXP]
}

/// Builds the SET PASSWORD request.
///
/// The password is sent XORed with the random number (repeated twice), so
/// the plain password is never transmitted.
///
/// The password is sent most significant byte first. In the wrong order the
/// tag does not answer at all (measured). The mask uses the two random bytes
/// in the order the tag sent them, repeated.
pub fn set_password_request(password: u32, random: u16) -> [u8; 8] {
    let password = password.to_be_bytes();
    let mask = random.to_le_bytes();
    [
        FLAGS,
        CMD_SET_PASSWORD,
        MFG_NXP,
        PASSWORD_ID_PRIVACY,
        password[0] ^ mask[0],
        password[1] ^ mask[1],
        password[2] ^ mask[0],
        password[3] ^ mask[1],
    ]
}

/// Builds the ENABLE PRIVACY request, which puts a tag back into privacy mode.
///
/// SL2S2602 §9.5.3.9, Table 39. The password is masked as in SET PASSWORD,
/// and there is **no password identifier byte**.
///
/// **Never addressed, on purpose.** DESTROY is one bit away at `B9h`, uses the
/// same mask, and on a new tag has the same password; but §9.5.3.8 says it
/// "can only be executed in addressed or selected mode". With the Address
/// flag clear, a one-bit mistake becomes a request the tag ignores. The tests
/// check this.
pub fn enable_privacy_request(password: u32, random: u16) -> [u8; 7] {
    let password = password.to_be_bytes();
    let mask = random.to_le_bytes();
    [
        FLAGS,
        CMD_ENABLE_PRIVACY,
        MFG_NXP,
        password[0] ^ mask[0],
        password[1] ^ mask[1],
        password[2] ^ mask[0],
        password[3] ^ mask[1],
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ISO 15693-3 §7.3.1: bit 6 of the request flags is the Address flag,
    /// which would require an eight-byte UID these three bytes do not carry.
    /// A tag does not answer a malformed request, which looks like a box with
    /// no figure.
    #[test]
    fn the_random_number_request_is_not_addressed_to_a_uid() {
        // Given: nothing to address it to yet

        // When
        let request = get_random_number_request();

        // Then
        assert_eq!(
            request,
            [0x02, 0xB2, 0x04],
            "the UID is what an unlock does not yet know"
        );
    }

    #[test]
    fn the_password_request_is_not_addressed_to_a_uid() {
        // Given
        let (password, random) = (0, 0);

        // When
        let request = set_password_request(password, random);

        // Then
        assert_eq!(&request[..3], &[0x02, 0xB3, 0x04]);
    }

    #[test]
    fn the_password_is_masked_with_the_random_number_in_both_halves() {
        // Given: with a zero password, the result is the mask itself
        let (password, random) = (0x0000_0000, 0xABCD);

        // When
        let request = set_password_request(password, random);

        // Then: the two random bytes in the order the tag sent them, repeated
        assert_eq!(&request[4..], &[0xCD, 0xAB, 0xCD, 0xAB]);
    }

    /// The mask is an exclusive or: where password and random number share a
    /// bit, it cancels. With bits that do not overlap, OR gives the same bytes
    /// and a wrong mask would pass; a wrong mask mutes the tag until its field
    /// is cycled.
    #[test]
    fn the_password_is_masked_by_exclusive_or() {
        // Given: the vendor default password, and a random number sharing
        // bits with it
        let (password, random) = (0x0F0F_0F0F, 0xABCD);

        // When
        let request = set_password_request(password, random);

        // Then: 0x0F ^ 0xCD and 0x0F ^ 0xAB, repeated
        assert_eq!(&request[4..], &[0xC2, 0xA4, 0xC2, 0xA4]);
    }

    /// The password is sent in written order (most significant byte first).
    /// In the other order the tag does not answer, which looks like a box with
    /// no figure.
    #[test]
    fn the_password_goes_on_the_air_most_significant_byte_first() {
        // Given: with no mask, the password shows through
        let (password, random) = (0x1122_3344, 0x0000);

        // When
        let request = set_password_request(password, random);

        // Then
        assert_eq!(&request[4..], &[0x11, 0x22, 0x33, 0x44]);
    }

    #[test]
    fn the_request_carries_the_privacy_password_identifier() {
        // Given
        let (password, random) = (1, 1);

        // When
        let request = set_password_request(password, random);

        // Then
        assert_eq!(request[3], PASSWORD_ID_PRIVACY);
    }

    /// The command code is written out, not taken from the constant: `0xB9`
    /// is the irreversible DESTROY, so a typo in the constant must be caught.
    #[test]
    fn the_privacy_request_carries_the_enable_privacy_command_and_not_destroy() {
        // Given
        let (password, random) = (0, 0);

        // When
        let request = enable_privacy_request(password, random);

        // Then
        assert_eq!(request[1], 0xBA, "0xB9 is DESTROY");
    }

    /// SL2S2602 §9.5.3.8 only allows DESTROY in addressed or selected mode, so
    /// an unaddressed frame can never destroy a tag, whatever its command
    /// byte. The Address flag must stay clear.
    #[test]
    fn the_privacy_request_is_never_addressed_to_a_uid() {
        // Given
        let (password, random) = (0, 0);

        // When
        let request = enable_privacy_request(password, random);

        // Then
        assert_eq!(
            &request[..3],
            &[0x02, 0xBA, 0x04],
            "the Address flag is what keeps a slip to DESTROY inert"
        );
    }

    /// Masked like SET PASSWORD: see `the_password_is_masked_by_exclusive_or`.
    #[test]
    fn the_privacy_request_masks_the_password_by_exclusive_or() {
        // Given
        let (password, random) = (0x0F0F_0F0F, 0xABCD);

        // When
        let request = enable_privacy_request(password, random);

        // Then
        assert_eq!(&request[3..], &[0xC2, 0xA4, 0xC2, 0xA4]);
    }

    /// No password identifier byte, unlike SET PASSWORD: in Table 39 the
    /// masked password follows the manufacturer code directly.
    #[test]
    fn the_privacy_request_masks_the_password_with_no_identifier_byte() {
        // Given: with no mask, the password shows through
        let (password, random) = (0x1122_3344, 0x0000);

        // When
        let request = enable_privacy_request(password, random);

        // Then
        assert_eq!(&request[3..], &[0x11, 0x22, 0x33, 0x44]);
        assert_eq!(request.len(), 7);
    }
}
