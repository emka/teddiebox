//! NXP ICODE privacy mode.
//!
//! Tonie figures ship with privacy mode enabled: the tag ignores inventory
//! entirely until it receives the correct privacy password. The unlock is a
//! two-step exchange — fetch a random number, then send the password XORed
//! with that number, twice over.
//!
//! **Verified against silicon on 2026-09-02**, on a Tonie figure and on a
//! SLIX-L, both of which answered inventory after being unlocked this way.
//!
//! Named for SLIX by habit, but **plain ICODE SLIX has no privacy mode at
//! all** — it offers only a password-protected EAS/AFI, at password
//! identifier `10h`. Privacy belongs to SLIX-L and SLIX2 (SL2S5002 §1.3), so
//! **SL2S2602, the SLIX2 product data sheet, is the reference for everything
//! here**; it is the only public one in the family
//! that carries the full command table.
//!
//! It settles two things this driver had to find out the hard way. Table 13
//! gives the password identifiers — `01h` read, `02h` write, **`04h`
//! privacy**, `08h` destroy, `10h` EAS/AFI. And §9.5.3.2: "the SET PASSWORD
//! command can only be executed in Addressed or Selected mode **except for
//! the Privacy password**", which is what makes the unaddressed request
//! below correct rather than a violation — a tag in privacy mode will not
//! give up the UID an addressed request would need.
//!
//! One rule from it governs everything above: **a password the tag does not
//! hold is answered with silence, and the tag then ignores every command
//! until its field has been taken away** (SL2S2602 §9.5.3.2). Trying a second
//! password without that reset asks a tag that has stopped listening.
//!
//! The Toniebox's own password is a caller-supplied constant, not held here.

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

/// NXP's factory default privacy password.
///
/// SL2S2602 §9.2.3, the configuration ICs are delivered in: "all password
/// bytes are 0Fh for the Privacy and Destroy passwords". A tag that has never
/// had a password written to it still holds this, so a plain one off the reel
/// is readable without knowing anything about it. Confirmed at the bench on
/// 2026-09-02: a SLIX-L that refused the Toniebox password accepted this and
/// then answered inventory with `E00403504E3D2C1B`.
///
/// It is a published default, not a secret, which is why it can live here
/// while the Toniebox's own password cannot.
///
/// **The Destroy password ships as the same value**, and DESTROY (`B9h`) sits
/// next to ENABLE PRIVACY (`BAh`). On a factory tag a one-bit slip in the
/// command code is therefore not a failed request — it is an irreversibly
/// dead tag. Neither command is implemented here, and neither should be added
/// without the request format in front of you.
pub const VENDOR_DEFAULT_PASSWORD: u32 = 0x0F0F_0F0F;

/// Builds the GET RANDOM NUMBER request.
pub fn get_random_number_request() -> [u8; 3] {
    [FLAGS, CMD_GET_RANDOM_NUMBER, MFG_NXP]
}

/// Builds the SET PASSWORD request.
///
/// The password is transmitted XORed with the random number repeated across
/// both halves, so the plaintext never crosses the air gap.
///
/// It goes on the air most significant byte first, which is how a password is
/// written down and how the box's is quoted. Sent the other way round the tag
/// does not refuse it, it simply says nothing at all — measured at the bench,
/// where reversing the byte order turned silence into the one-byte `0x00`
/// that ISO 15693 uses to mean "done". The mask is applied in that same wire
/// order: the two random bytes as the tag sent them, repeated.
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
        // A zero password leaves the mask itself: the two random bytes in the
        // order the tag sent them, repeated.
        assert_eq!(&request[4..], &[0xCD, 0xAB, 0xCD, 0xAB]);
    }

    /// The box's own password, as it is written down, is what must appear on
    /// the air. Sent least significant byte first the tag stays silent — the
    /// one failure that reads as an empty plate.
    #[test]
    fn the_password_goes_on_the_air_most_significant_byte_first() {
        let request = set_password_request(0x1122_3344, 0x0000);
        assert_eq!(&request[4..], &[0x11, 0x22, 0x33, 0x44]);
    }

    #[test]
    fn the_request_carries_the_privacy_password_identifier() {
        let request = set_password_request(1, 1);
        assert_eq!(request[3], PASSWORD_ID_PRIVACY);
    }
}
