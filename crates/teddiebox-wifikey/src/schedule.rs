//! When to derive a key: only for credentials a successful join has just
//! used, only while nothing plays, and a slice at a time.
//!
//! **Only the credentials that joined count.** A successful join proves the
//! SSID and passphrase it used. Credentials typed or re-read afterwards are
//! not proven; storing a key for them could store one the router refuses,
//! causing a failed join on every later boot.

use teddiebox_core::checksum::Crc32;

use crate::psk::{Derivation, Psk};

/// What one pass came to.
#[derive(Debug, PartialEq, Eq)]
pub enum Pass {
    /// Nothing to do, or paused.
    Idle,
    /// A slice ran; more to come.
    Working,
    /// The key for the proven credentials, returned once.
    Done(Psk),
}

/// Which credentials something is about, without keeping the passphrase.
fn fingerprint(ssid: &[u8], passphrase: &[u8]) -> u32 {
    let mut crc = Crc32::new();
    crc.update(ssid);
    crc.update(&[0]);
    crc.update(passphrase);
    crc.finish()
}

/// The proven credentials, and their key derivation if one has started.
pub struct Schedule {
    proved: Option<u32>,
    job: Option<Derivation>,
}

impl Default for Schedule {
    fn default() -> Self {
        Self::new()
    }
}

impl Schedule {
    pub const fn new() -> Self {
        Self {
            proved: None,
            job: None,
        }
    }

    /// A join with this passphrase succeeded, so its key is worth keeping.
    ///
    /// Replaces any earlier credentials and drops their derivation.
    pub fn proved(&mut self, ssid: &[u8], passphrase: &[u8]) {
        let fingerprint = fingerprint(ssid, passphrase);
        if self.proved != Some(fingerprint) {
            self.job = None;
        }
        self.proved = Some(fingerprint);
    }

    /// One idle pass: `current` is what the box would join with now, and
    /// `stored` says whether a key for it is already kept.
    pub fn pass(
        &mut self,
        current: Option<(&[u8], &[u8])>,
        stored: bool,
        playing: bool,
        slice: u32,
    ) -> Pass {
        if playing {
            return Pass::Idle;
        }
        let Some((ssid, passphrase)) = current else {
            return Pass::Idle;
        };
        if stored || self.proved != Some(fingerprint(ssid, passphrase)) {
            return Pass::Idle;
        }
        let job = self
            .job
            .get_or_insert_with(|| Derivation::new(ssid, passphrase));
        match job.step(slice) {
            None => Pass::Working,
            Some(psk) => {
                self.job = None;
                self.proved = None;
                Pass::Done(psk)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SLICE: u32 = 1000;
    const IEEE: [u8; 32] = [
        0xf4, 0x2c, 0x6f, 0xc5, 0x2d, 0xf0, 0xeb, 0xef, 0x9e, 0xbb, 0x4b, 0x90, 0xb3, 0x8a, 0x5f,
        0x90, 0x2e, 0x83, 0xfe, 0x1b, 0x13, 0x5a, 0x70, 0xe2, 0x3a, 0xed, 0x76, 0x2e, 0x97, 0x10,
        0xa1, 0x2e,
    ];
    const IEEE_SECOND: [u8; 32] = [
        0x0d, 0xc0, 0xd6, 0xeb, 0x90, 0x55, 0x5e, 0xd6, 0x41, 0x97, 0x56, 0xb9, 0xa1, 0x5e, 0xc3,
        0xe3, 0x20, 0x9b, 0x63, 0xdf, 0x70, 0x7d, 0xd5, 0x08, 0xd1, 0x45, 0x81, 0xf8, 0x98, 0x27,
        0x21, 0xaf,
    ];

    fn idle(s: &mut Schedule, ssid: &[u8], passphrase: &[u8]) -> Pass {
        s.pass(Some((ssid, passphrase)), false, false, SLICE)
    }

    /// Runs passes until a key comes out, or returns `None` after far more
    /// passes than needed.
    fn until_done(s: &mut Schedule, ssid: &[u8], passphrase: &[u8]) -> Option<(u32, Psk)> {
        for n in 1..=100 {
            if let Pass::Done(psk) = idle(s, ssid, passphrase) {
                return Some((n, psk));
            }
        }
        None
    }

    #[test]
    fn nothing_is_derived_before_a_passphrase_join() {
        // Given
        let mut s = Schedule::new();

        // When
        let derived = until_done(&mut s, b"IEEE", b"password");

        // Then
        assert_eq!(derived, None);
    }

    #[test]
    fn proved_credentials_derive_their_key() {
        // Given
        let mut s = Schedule::new();
        s.proved(b"IEEE", b"password");

        // When
        let (_, psk) = until_done(&mut s, b"IEEE", b"password").expect("derives");

        // Then
        assert_eq!(psk.as_bytes(), &IEEE);
    }

    #[test]
    fn credentials_changed_since_the_join_are_not_derived() {
        // Given
        let mut s = Schedule::new();
        s.proved(b"IEEE", b"password");

        // When
        let derived = until_done(&mut s, b"IEEE", b"guessed");

        // Then
        assert_eq!(derived, None);
    }

    #[test]
    fn a_stored_key_for_these_credentials_is_not_derived_again() {
        // Given
        let mut s = Schedule::new();
        s.proved(b"IEEE", b"password");

        // When
        let passes = [(); 100].map(|_| s.pass(Some((b"IEEE", b"password")), true, false, SLICE));

        // Then
        assert!(passes.iter().all(|pass| *pass == Pass::Idle));
    }

    #[test]
    fn playing_pauses_the_derivation() {
        // Given: two slices done
        let mut s = Schedule::new();
        s.proved(b"IEEE", b"password");
        idle(&mut s, b"IEEE", b"password");
        idle(&mut s, b"IEEE", b"password");

        // When
        let paused = [(); 10].map(|_| s.pass(Some((b"IEEE", b"password")), false, true, SLICE));

        // Then
        assert!(paused.iter().all(|pass| *pass == Pass::Idle));
    }

    #[test]
    fn a_paused_derivation_resumes_where_it_stopped() {
        // Given: two slices done, then paused while a story played
        let mut s = Schedule::new();
        s.proved(b"IEEE", b"password");
        idle(&mut s, b"IEEE", b"password");
        idle(&mut s, b"IEEE", b"password");
        s.pass(Some((b"IEEE", b"password")), false, true, SLICE);

        // When
        let resumed = until_done(&mut s, b"IEEE", b"password").map(|(n, _)| n);

        // Then: 4095 rounds at 1000 a pass is five passes in all, two already
        // done before the pause
        assert_eq!(resumed, Some(3));
    }

    #[test]
    fn newly_proved_credentials_start_over_with_their_own_key() {
        // Given: one slice done for the first credentials, then others proved
        let mut s = Schedule::new();
        s.proved(b"IEEE", b"password");
        idle(&mut s, b"IEEE", b"password");
        s.proved(b"ThisIsASSID", b"ThisIsAPassword");

        // When
        let (n, psk) = until_done(&mut s, b"ThisIsASSID", b"ThisIsAPassword").expect("derives");

        // Then: all five passes, from the start
        assert_eq!(n, 5);
        assert_eq!(psk.as_bytes(), &IEEE_SECOND);
    }

    #[test]
    fn a_derived_key_is_handed_out_once() {
        // Given
        let mut s = Schedule::new();
        s.proved(b"IEEE", b"password");
        until_done(&mut s, b"IEEE", b"password").expect("derives");

        // When
        let again = until_done(&mut s, b"IEEE", b"password");

        // Then
        assert_eq!(again, None);
    }

    #[test]
    fn no_credentials_derive_nothing() {
        // Given
        let mut s = Schedule::new();
        s.proved(b"IEEE", b"password");

        // When
        let pass = s.pass(None, false, false, SLICE);

        // Then
        assert_eq!(pass, Pass::Idle);
    }
}
