#![no_std]

//! The setup portal's wire formats: the one page it serves, the form it takes
//! back, and the four DHCP messages it answers.
//!
//! Everything here is bytes in and bytes out, so it runs on the host. What
//! needs the bench — the access point, the sockets, the card — lives in
//! `firmware/src/portal.rs` and not here.

#[cfg(test)]
extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod dhcp;
pub mod form;
pub mod http;
pub mod page;
pub mod submission;

/// The largest `CONFIG.TXT` this box will read or write.
///
/// A realistic file — a 32-character SSID, a 63-character passphrase, a
/// server and a 128-character `update_url` — lands under 400 bytes. This is
/// roomy without being an invitation to paste something else in.
///
/// A cap on the *file*, and deliberately not on the request that carries it:
/// see [`MAX_BODY`].
pub const MAX_CONFIG: usize = 1024;

/// The largest request body this box will accept.
///
/// Three times [`MAX_CONFIG`] plus room for the `config=` prefix, because a
/// form body is not the file — it is the file percent-encoded, and every byte
/// outside `[A-Za-z0-9*-._]` becomes three. A newline becomes `%0D%0A`, six
/// bytes for two; `=`, `:` and `/` become three each. Measured on a realistic
/// config (ssid, a passphrase with `#` and `!` in it, server, `update_url`,
/// two flags): 175 bytes of file arrive as 237, a ratio of 1.35. The worst
/// case is a file of nothing but encoded bytes, at 3x.
///
/// Capping the body at [`MAX_CONFIG`] instead — which this did until the
/// portal was wired up — refuses any file over about 760 bytes on the way
/// back in while still *displaying* it, so the box shows somebody a config it
/// will not let them keep and blames a number they never typed. The two
/// quantities were never the same thing.
///
/// A file that is genuinely too long is still refused, by
/// [`form::field::<MAX_CONFIG>`](form::field) failing to decode into its
/// capacity — a refusal about the file, which is the thing somebody can act
/// on.
pub const MAX_BODY: usize = MAX_CONFIG * 3 + 16;
