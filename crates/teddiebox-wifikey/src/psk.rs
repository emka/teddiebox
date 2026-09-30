//! WPA2's PSK: PBKDF2-HMAC-SHA1(passphrase, ssid, 4096, 32 bytes).
//!
//! **Done in slices**, because the box has only one executor. `step` runs a
//! chosen number of rounds and returns, so the NFC reader and the codec get
//! a turn in between. Both output blocks advance together, so one round is
//! two HMACs.

use hmac::{Hmac, KeyInit, Mac};
use sha1::Sha1;

type HmacSha1 = Hmac<Sha1>;

/// Rounds WPA2 fixes for the derivation.
pub const ITERATIONS: u32 = 4096;

/// A derived key. Never printed.
#[derive(Clone, PartialEq, Eq)]
pub struct Psk([u8; 32]);

impl Psk {
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// The key in the form the driver takes: 64 lower-case hex characters,
    /// which it uses as the key directly instead of deriving one.
    pub fn hex(&self) -> [u8; 64] {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        let mut out = [0; 64];
        for (i, byte) in self.0.iter().enumerate() {
            out[2 * i] = DIGITS[usize::from(byte >> 4)];
            out[2 * i + 1] = DIGITS[usize::from(byte & 0x0f)];
        }
        out
    }
}

impl core::fmt::Debug for Psk {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("Psk(**)")
    }
}

/// One PBKDF2 output block in progress: the last `U` and the running XOR.
struct Block {
    u: [u8; 20],
    t: [u8; 20],
}

/// A derivation part-way through.
pub struct Derivation {
    mac: HmacSha1,
    blocks: [Block; 2],
    done: u32,
}

impl Derivation {
    pub fn new(ssid: &[u8], passphrase: &[u8]) -> Self {
        let mac = HmacSha1::new_from_slice(passphrase).expect("HMAC takes a key of any length");
        let first = |index: u32| -> [u8; 20] {
            let mut m = mac.clone();
            m.update(ssid);
            m.update(&index.to_be_bytes());
            m.finalize().into_bytes().into()
        };
        let (u1, u2) = (first(1), first(2));
        Self {
            mac,
            blocks: [Block { u: u1, t: u1 }, Block { u: u2, t: u2 }],
            done: 1,
        }
    }

    /// Runs up to `iterations` more rounds; the key once all are done.
    pub fn step(&mut self, iterations: u32) -> Option<Psk> {
        let mut left = iterations;
        while self.done < ITERATIONS && left > 0 {
            for block in &mut self.blocks {
                let mut m = self.mac.clone();
                m.update(&block.u);
                block.u = m.finalize().into_bytes().into();
                for (t, u) in block.t.iter_mut().zip(block.u) {
                    *t ^= u;
                }
            }
            self.done += 1;
            left -= 1;
        }
        (self.done == ITERATIONS).then(|| {
            let mut key = [0; 32];
            key[..20].copy_from_slice(&self.blocks[0].t);
            key[20..].copy_from_slice(&self.blocks[1].t[..12]);
            Psk(key)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn whole(ssid: &[u8], passphrase: &[u8]) -> [u8; 32] {
        *Derivation::new(ssid, passphrase)
            .step(ITERATIONS)
            .expect("a whole budget finishes")
            .as_bytes()
    }

    fn hex(s: &str) -> [u8; 32] {
        let mut out = [0; 32];
        for (i, byte) in out.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap();
        }
        out
    }

    /// IEEE 802.11i-2004 Annex H.4; values re-derived with Python's hashlib.
    const IEEE_KEY: &str = "f42c6fc52df0ebef9ebb4b90b38a5f902e83fe1b135a70e23aed762e9710a12e";

    #[test]
    fn derives_the_ieee_vector() {
        // Given
        let (ssid, passphrase) = (b"IEEE", b"password");

        // When
        let key = whole(ssid, passphrase);

        // Then
        assert_eq!(key, hex(IEEE_KEY));
    }

    #[test]
    fn derives_the_second_ieee_vector() {
        // Given
        let (ssid, passphrase) = (b"ThisIsASSID", b"ThisIsAPassword");

        // When
        let key = whole(ssid, passphrase);

        // Then
        assert_eq!(
            key,
            hex("0dc0d6eb90555ed6419756b9a15ec3e3209b63df707dd508d14581f8982721af")
        );
    }

    #[test]
    fn derives_for_the_longest_ssid() {
        // Given
        let (ssid, passphrase) = ([b'Z'; 32], [b'a'; 32]);

        // When
        let key = whole(&ssid, &passphrase);

        // Then
        assert_eq!(
            key,
            hex("becb93866bb8c3832cb777c2f559807c8c59afcb6eae734885001300a981cc62")
        );
    }

    /// `python3 -c "import hashlib;print(hashlib.pbkdf2_hmac('sha1',b'p'*63,b'IEEE',4096,32).hex())"`
    #[test]
    fn derives_for_the_longest_passphrase() {
        // Given
        let (ssid, passphrase) = (b"IEEE", [b'p'; 63]);

        // When
        let key = whole(ssid, &passphrase);

        // Then
        assert_eq!(
            key,
            hex("4fce3de309b0d3e56b403791a4dcc712417e575546d16dd6024265cd2faee56e")
        );
    }

    #[test]
    fn a_short_budget_is_not_a_key() {
        // Given
        let mut d = Derivation::new(b"IEEE", b"password");

        // When
        let key = d.step(ITERATIONS - 2);

        // Then
        assert!(key.is_none());
    }

    #[test]
    fn slices_reach_the_same_key_as_one_step() {
        // Given
        let mut d = Derivation::new(b"IEEE", b"password");

        // When: slices of seven rounds until the key is ready
        let mut got = None;
        for _ in 0..ITERATIONS {
            got = d.step(7);
            if got.is_some() {
                break;
            }
        }

        // Then
        assert_eq!(*got.expect("finishes").as_bytes(), hex(IEEE_KEY));
    }

    #[test]
    fn a_finished_derivation_keeps_answering() {
        // Given
        let mut d = Derivation::new(b"IEEE", b"password");
        let first = d.step(ITERATIONS + 100).expect("finishes");

        // When
        let again = d.step(0);

        // Then
        assert_eq!(again.expect("still finished").as_bytes(), first.as_bytes());
    }

    #[test]
    fn hex_is_what_the_driver_takes() {
        // Given
        let psk = Psk::from_bytes(hex(IEEE_KEY));

        // When
        let text = psk.hex();

        // Then
        assert_eq!(&text, IEEE_KEY.as_bytes());
    }

    #[test]
    fn debug_never_shows_the_key() {
        // Given
        extern crate std;
        let psk = Psk::from_bytes([0xab; 32]);

        // When
        let shown = std::format!("{psk:?}");

        // Then
        assert!(!shown.contains("ab"), "{shown}");
    }
}
