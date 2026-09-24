#![no_std]
//! The Wi-Fi key the box hands its radio, and the flash record that keeps it.
//!
//! The driver derives a WPA2 key from the passphrase on every join —
//! 4096 rounds of HMAC-SHA1, 1.75 s of the 1.8 s a join took on 2026-09-24.
//! Handed the derived key instead, the same join took 64–72 ms. So the box
//! derives it once, keeps it, and derives again only when the credentials
//! change.

pub mod psk;
pub mod record;
