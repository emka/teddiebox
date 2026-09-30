#![no_std]

//! The setup portal's wire formats: the one page it serves, the config form
//! and the uploaded file it takes back, and the four DHCP messages it answers.
//!
//! Everything here is bytes in and bytes out, so it can be tested on the host.
//! The parts that need hardware — the access point, the sockets, the SD card —
//! live in `firmware/src/portal.rs`.

#[cfg(test)]
extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod dhcp;
pub mod form;
pub mod http;
pub mod multipart;
pub mod page;
pub mod submission;

/// The largest `CONFIG.TXT` this box will read or write.
///
/// A realistic file — a 32-character SSID, a 63-character passphrase, a
/// server and a 128-character `update_url` — is under 400 bytes, so this
/// leaves plenty of room.
///
/// This limits the *file*, not the request that carries it: see
/// [`MAX_BODY`].
pub const MAX_CONFIG: usize = 1024;

/// The largest request body this box will accept.
///
/// Three times [`MAX_CONFIG`] plus room for the `config=` prefix. The form
/// body is the file percent-encoded, and every byte outside
/// `[A-Za-z0-9*-._]` becomes three bytes. A realistic config grows by about
/// 1.35x; a file made only of encoded bytes grows by 3x.
///
/// If the body were capped at [`MAX_CONFIG`], a file of about 760 bytes could
/// be shown on the page but not saved back.
///
/// A file that really is too long is still refused, when
/// [`form::field::<MAX_CONFIG>`](form::field) cannot decode it into its
/// capacity.
pub const MAX_BODY: usize = MAX_CONFIG * 3 + 16;
