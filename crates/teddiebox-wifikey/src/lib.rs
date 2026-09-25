#![no_std]
//! The Wi-Fi key the box hands its radio, and the flash record that keeps it.
//!
//! Turning a passphrase into a WPA2 key takes 4096 rounds of HMAC-SHA1:
//! about 1.75 s of a 1.8 s join. Given the finished key instead, a join
//! takes 64–72 ms. So the box derives the key once, stores it, and derives it
//! again only when the credentials change.

pub mod psk;
pub mod record;
pub mod schedule;
