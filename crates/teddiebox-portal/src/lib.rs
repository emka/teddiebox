#![no_std]

//! The setup portal's wire formats: the one page it serves, the form it takes
//! back, and the four DHCP messages it answers.
//!
//! Everything here is bytes in and bytes out, so it runs on the host. What
//! needs the bench — the access point, the sockets, the card — lives in
//! `firmware/src/portal.rs` and not here.

pub mod form;

/// The largest `CONFIG.TXT` this box will read or write, and the largest body
/// it will accept.
///
/// A realistic file — a 32-character SSID, a 63-character passphrase, a
/// server and a 128-character `update_url` — lands under 400 bytes. This is
/// roomy without being an invitation to paste something else in.
pub const MAX_CONFIG: usize = 1024;
